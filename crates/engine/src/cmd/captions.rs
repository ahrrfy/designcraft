//! Object › Captions › Generate Static Caption: a text frame beside each selected object, filled
//! from its metadata.

use designcraft_doc::{Document, Item, ItemId, ParaFormat, Selection};
use designcraft_geom::Rect;
use serde_json::{Value, json};

use super::object::RemoveKeep;
use super::{CommandSpec, bad, cmd, has_selection, str_param, targets};
use crate::{Result, Session};

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "object.caption",
        "Generate Static Caption",
        ["Object", "Captions"],
        None,
        "{text?: template (default \"{name}\"; variables {name} {path} {altText} {label} {ppi} {dimensions} {format}), position?: below|above|left|right, offset? (pt, 0), height? (pt, 24), style?: paragraph style, live?: bool (kept current as the source changes), group?: bool (group each source object with its caption), captionFrame?: existing text frame id, ids?} → {ids, groups?}",
        has_selection,
        caption
    )]
}

/// The metadata variables of `it`.
fn variable(d: &Document, it: &Item, xf: designcraft_geom::Affine, name: &str) -> String {
    let g = it.graphic();
    let asset = g.and_then(|g| d.assets.get(&g.asset));
    match name {
        "name" => asset.map(|a| a.name.clone()).unwrap_or_default(),
        "path" => asset.and_then(|a| a.link.clone()).unwrap_or_default(),
        "altText" => it.alt_text.clone(),
        "label" => it.label.clone(),
        "format" => asset.map(|a| a.mime.rsplit('/').next().unwrap_or("").to_uppercase()).unwrap_or_default(),
        "dimensions" => asset.and_then(|a| a.pixels).map(|(w, h)| format!("{w} × {h}")).unwrap_or_default(),
        "ppi" => match (g, asset.and_then(|a| a.pixels)) {
            (Some(g), Some((w, _))) if g.size.0 > 0.0 => {
                let m = (xf * g.xf).as_coeffs();
                let scale = m[0].hypot(m[1]);
                let inches = g.size.0 * scale / 72.0;
                if inches > 0.0 { format!("{}", (w as f64 / inches).round()) } else { String::new() }
            }
            _ => String::new(),
        },
        _ => String::new(),
    }
}

pub(crate) fn fill(d: &Document, it: &Item, xf: designcraft_geom::Affine, template: &str) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        match rest[i..].find('}') {
            Some(j) => {
                out.push_str(&variable(d, it, xf, &rest[i + 1..i + j]));
                rest = &rest[i + j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn caption(s: &mut Session, p: &Value) -> Result<Value> {
    const ID: &str = "object.caption";
    let ids = targets(s, p)?;
    let template = str_param(p, "text").unwrap_or("{name}").to_string();
    let pos = str_param(p, "position").unwrap_or("below").to_string();
    if !["below", "above", "left", "right"].contains(&pos.as_str()) {
        return Err(bad(ID, format!("unknown position `{pos}`")));
    }
    let offset = p.get("offset").and_then(Value::as_f64).unwrap_or(0.0);
    let height = p.get("height").and_then(Value::as_f64).unwrap_or(24.0).max(1.0);
    let style = str_param(p, "style").map(str::to_string);
    let live = p.get("live").and_then(Value::as_bool).unwrap_or(false);
    let group_with_source = p.get("group").and_then(Value::as_bool).unwrap_or(false);
    let existing_caption_frame = match p.get("captionFrame") {
        None | Some(Value::Null) => None,
        Some(v) => Some(ItemId(v.as_u64().filter(|&n| n >= 1).ok_or_else(|| bad(ID, "captionFrame must be a positive item id"))?)),
    };
    if let Some(st) = &style
        && s.doc()?.doc.styles.para(st).is_none()
    {
        return Err(bad(ID, format!("no paragraph style `{st}`")));
    }
    let lid = s.doc()?.active_layer;
    s.edit(|d, sel| {
        let mut unique_ids = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for id in &ids {
            if seen.insert(*id) {
                unique_ids.push(*id);
            }
        }
        if let Some(cid) = existing_caption_frame {
            if unique_ids.len() != 1 {
                return Err(bad(ID, "captionFrame requires exactly one target object"));
            }
            let source = unique_ids[0];
            if cid == source {
                return Err(bad(ID, "captionFrame cannot be its own source"));
            }
            let src_loc = d.find(source).ok_or_else(|| bad(ID, "nothing to caption"))?;
            let cap_loc = d.find(cid).ok_or_else(|| bad(ID, "captionFrame not found"))?;
            if cap_loc.spread != src_loc.spread {
                return Err(bad(ID, "captionFrame must be on the same spread as its source"));
            }
            let story_id = d.item(cid).and_then(|it| it.text_frame()).ok_or_else(|| bad(ID, "captionFrame must be a text frame"))?.story;
            let src_it = d.item_at(&src_loc).ok_or_else(|| bad(ID, "nothing to caption"))?;
            let text = fill(d, src_it, d.parent_xf(&src_loc) * src_it.xf, &template);
            if let Some(st) = d.story_mut(story_id) {
                let len = st.len();
                st.replace(0..len, &text);
                if let Some(style_name) = &style {
                    for p_run in &mut st.paras {
                        p_run.style = style_name.clone();
                    }
                }
            }
            if let Some(c) = d.item_mut(cid) {
                c.live_caption = live.then_some(designcraft_doc::LiveCaption { source, template });
            }
            if group_with_source {
                let gid = super::object::group_items_in_doc(d, ID, &[source, cid])?;
                *sel = Selection::items(vec![gid]);
                return Ok(json!({"ids": [cid.0], "groups": [gid.0]}));
            }
            *sel = Selection::items(vec![cid]);
            return Ok(json!({"ids": [cid.0]}));
        }
        let mut made = Vec::new();
        let mut groups = Vec::new();
        for source in unique_ids {
            let Some(loc) = d.find(source) else { continue };
            let Some(it) = d.item_at(&loc) else { continue };
            let parent_xf = d.parent_xf(&loc);
            let xf = parent_xf * it.xf;
            let b = xf.transform_rect_bbox(it.inner_bounds());
            let text = fill(d, it, xf, &template);
            let target_layer = if group_with_source { it.layer } else { lid };
            let r = match pos.as_str() {
                "above" => Rect::new(b.x0, b.y0 - offset - height, b.x1, b.y0 - offset),
                "left" => Rect::new(b.x0 - offset - 144.0, b.y0, b.x0 - offset, b.y1),
                "right" => Rect::new(b.x1 + offset, b.y0, b.x1 + offset + 144.0, b.y1),
                _ => Rect::new(b.x0, b.y1 + offset, b.x1, b.y1 + offset + height),
            };
            let para = ParaFormat { style: style.clone().unwrap_or_else(|| d.styles.default_paragraph.clone()), ..Default::default() };
            let (cid, _) = d.add_text_frame(loc.spread, r, target_layer, &text, para)?;
            if live && let Some(c) = d.item_mut(cid) {
                c.live_caption = Some(designcraft_doc::LiveCaption { source, template: template.clone() });
            }
            if group_with_source {
                if loc.path.len() > 1 {
                    let parent_path = &loc.path[..loc.path.len() - 1];
                    let det = parent_xf.determinant();
                    if !(det.is_finite() && det.abs() > 1e-9) {
                        return Err(bad(ID, "parent group transform is non-invertible"));
                    }
                    let mut cap_item = d.remove_item_keep_story(cid)?;
                    cap_item.xf = parent_xf.inverse() * cap_item.xf;
                    let siblings = super::object::siblings_mut(d, loc.spread, parent_path).ok_or_else(|| bad(ID, "parent group not found"))?;
                    let insert_at = loc.path.last().map_or(siblings.len(), |idx| idx.saturating_add(1).min(siblings.len()));
                    siblings.insert(insert_at, std::sync::Arc::new(cap_item));
                }
                let gid = super::object::group_items_in_doc(d, ID, &[source, cid])?;
                groups.push(gid);
            }
            made.push(cid);
        }
        if made.is_empty() {
            return Err(bad(ID, "nothing to caption"));
        }
        let made_ids: Vec<u64> = made.iter().map(|i: &ItemId| i.0).collect();
        if group_with_source {
            let group_ids: Vec<u64> = groups.iter().map(|i: &ItemId| i.0).collect();
            *sel = Selection::items(groups);
            Ok(json!({"ids": made_ids, "groups": group_ids}))
        } else {
            *sel = Selection::items(made);
            Ok(json!({"ids": made_ids}))
        }
    })
}

/// Whether any item on a document spread (including inside groups) has a live caption.
pub(crate) fn has_live_captions(d: &Document) -> bool {
    let mut found = false;
    for sp in &d.spreads {
        for it in &sp.items {
            it.walk(&mut |node| {
                if node.live_caption.is_some() {
                    found = true;
                }
            });
            if found {
                return true;
            }
        }
    }
    false
}

/// Bring live captions up to date with their source objects (the text frame's story is
/// replaced when its text changed; a caption whose source is gone keeps its last text).
pub(crate) fn refresh_live(d: &mut Document) -> bool {
    let mut todo = Vec::new();
    for id in d.all_items() {
        let Some(c) = d.item(id) else { continue };
        let (Some(lc), Some(tf)) = (&c.live_caption, c.text_frame()) else { continue };
        let (Some(loc), Some(src)) = (d.find(lc.source), d.item(lc.source)) else { continue };
        let text = fill(d, src, d.parent_xf(&loc) * src.xf, &lc.template);
        if d.story(tf.story).is_some_and(|st| st.text != text) {
            todo.push((tf.story, text));
        }
    }
    let changed = !todo.is_empty();
    for (sid, text) in todo {
        if let Some(st) = d.story_mut(sid) {
            let len = st.len();
            st.replace(0..len, &text);
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::Session;

    #[test]
    fn static_caption_from_metadata() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let a = s.execute("frame.create", &json!({"rect": [72, 72, 272, 172]})).unwrap()["id"].as_u64().unwrap();
        s.execute("object.altText", &json!({"text": "Harbour at dawn", "ids": [a]})).unwrap();
        s.execute("object.label", &json!({"label": "fig-1", "ids": [a]})).unwrap();
        let r = s.execute("object.caption", &json!({"ids": [a], "text": "Figure {label}: {altText}", "offset": 6})).unwrap();
        let cid = designcraft_doc::ItemId(r["ids"][0].as_u64().unwrap());
        let d = &s.doc().unwrap().doc;
        let c = d.item(cid).unwrap();
        let b = c.bounds();
        assert_eq!((b.x0, b.y0, b.x1), (72.0, 178.0, 272.0));
        let sid = c.text_frame().unwrap().story;
        assert_eq!(d.stories[&sid].text, "Figure fig-1: Harbour at dawn");
        assert!(s.execute("object.caption", &json!({"ids": [a], "position": "sideways"})).is_err());
    }

    #[test]
    fn live_caption_follows_its_source() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let a = s.execute("frame.create", &json!({"rect": [72, 72, 272, 172]})).unwrap()["id"].as_u64().unwrap();
        s.execute("object.altText", &json!({"text": "First", "ids": [a]})).unwrap();
        let r = s.execute("object.caption", &json!({"ids": [a], "text": "Photo: {altText}", "live": true})).unwrap();
        let cid = designcraft_doc::ItemId(r["ids"][0].as_u64().unwrap());
        let text = |s: &Session| {
            let d = &s.doc().unwrap().doc;
            d.stories[&d.item(cid).unwrap().text_frame().unwrap().story].text.clone()
        };
        assert_eq!(text(&s), "Photo: First");
        s.execute("object.altText", &json!({"text": "Second", "ids": [a]})).unwrap();
        assert_eq!(text(&s), "Photo: Second", "updated with the source");
        s.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(text(&s), "Photo: First", "one undo step for both");
    }

    #[test]
    fn grouped_live_caption_binds_graphic_and_caption_atomically_and_moves_together() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let a = s.execute("frame.create", &json!({"rect": [72, 72, 272, 172], "content": "graphic"})).unwrap()["id"].as_u64().unwrap();
        s.execute("object.altText", &json!({"text": "أسد الغابة", "ids": [a]})).unwrap();
        let undo_before = s.doc().unwrap().history.undo.len();

        let r = s
            .execute("object.caption", &json!({"ids": [a, a], "text": "{altText}", "offset": 6, "height": 24, "live": true, "group": true}))
            .unwrap();
        assert_eq!(r["ids"].as_array().unwrap().len(), 1, "duplicate target ids are captioned once");
        assert_eq!(r["groups"].as_array().unwrap().len(), 1);
        let cid = designcraft_doc::ItemId(r["ids"][0].as_u64().unwrap());
        let gid = designcraft_doc::ItemId(r["groups"][0].as_u64().unwrap());

        assert_eq!(s.doc().unwrap().history.undo.len(), undo_before + 1, "caption + group is one undo step");
        assert_eq!(s.doc().unwrap().selection.items, vec![gid], "the atomic group is selected");
        assert_eq!(s.doc().unwrap().doc.spreads[0].items.len(), 1, "top-level spread holds the single group");

        // Moving the group moves both the graphic frame and the live caption frame together:
        s.execute("transform.move", &json!({"ids": [gid.0], "dx": 40.0, "dy": 20.0})).unwrap();
        let d = &s.doc().unwrap().doc;
        let src_loc = d.find(designcraft_doc::ItemId(a)).unwrap();
        let cap_loc = d.find(cid).unwrap();
        assert_eq!(src_loc.path.len(), 2, "source graphic is inside group");
        assert_eq!(cap_loc.path.len(), 2, "caption text frame is inside group");
        let src_bounds = (d.parent_xf(&src_loc) * d.item(designcraft_doc::ItemId(a)).unwrap().xf)
            .transform_rect_bbox(d.item(designcraft_doc::ItemId(a)).unwrap().inner_bounds());
        let cap_bounds = (d.parent_xf(&cap_loc) * d.item(cid).unwrap().xf).transform_rect_bbox(d.item(cid).unwrap().inner_bounds());
        assert!((src_bounds.x0 - 112.0).abs() < 1e-4 && (src_bounds.y0 - 92.0).abs() < 1e-4);
        assert!((cap_bounds.x0 - 112.0).abs() < 1e-4 && (cap_bounds.y0 - 198.0).abs() < 1e-4);

        // Live metadata binding continues to update the caption inside the group:
        s.execute("object.altText", &json!({"text": "غزال الصحراء", "ids": [a]})).unwrap();
        let cap_sid = s.doc().unwrap().doc.item(cid).unwrap().text_frame().unwrap().story;
        assert_eq!(s.doc().unwrap().doc.stories[&cap_sid].text, "غزال الصحراء");

        // Undo altText, undo move, undo grouped caption restores the ungrouped graphic frame:
        s.execute("edit.undo", &json!({})).unwrap();
        s.execute("edit.undo", &json!({})).unwrap();
        s.execute("edit.undo", &json!({})).unwrap();
        let d_after = &s.doc().unwrap().doc;
        assert!(d_after.item(gid).is_none());
        assert!(d_after.item(cid).is_none());
        assert!(d_after.item(designcraft_doc::ItemId(a)).is_some());
    }

    #[test]
    fn existing_styled_caption_frame_binds_live_and_preserves_formatting() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let art = s.execute("frame.create", &json!({"rect": [52, 118, 168, 224], "content": "graphic"})).unwrap()["id"].as_u64().unwrap();
        let cap = s.execute("frame.create", &json!({"rect": [58, 232, 162, 248], "content": "text", "text": "placeholder", "caret": false})).unwrap()
            ["id"]
            .as_u64()
            .unwrap();
        s.execute("type.char", &json!({"attrs": {"fontFamily": "Tahoma", "fontStyle": "Bold", "size": 11.0}})).unwrap();
        s.execute("type.para", &json!({"attrs": {"align": "center", "direction": "rightToLeft"}})).unwrap();
        let card_group = s.execute("object.group", &json!({"ids": [art, cap]})).unwrap()["id"].as_u64().unwrap();
        s.execute("object.altText", &json!({"ids": [art], "text": "صدق"})).unwrap();
        let r = s.execute("object.caption", &json!({"ids": [art], "captionFrame": cap, "text": "{altText}", "live": true})).unwrap();
        assert_eq!(r["ids"][0].as_u64().unwrap(), cap);

        let d = &s.doc().unwrap().doc;
        let cap_item = d.item(designcraft_doc::ItemId(cap)).unwrap();
        assert!(cap_item.live_caption.is_some());
        let st = &d.stories[&cap_item.text_frame().unwrap().story];
        assert_eq!(st.text, "صدق");
        assert_eq!(st.chars[0].format.over.font_family.as_deref(), Some("Tahoma"));
        assert_eq!(st.chars[0].format.over.font_style.as_deref(), Some("Bold"));
        assert_eq!(st.paras[0].para.direction, Some(designcraft_doc::TextDirection::RightToLeft));

        // Updating altText updates the bound caption inside the atomic card group:
        s.execute("object.altText", &json!({"ids": [art], "text": "أمانة"})).unwrap();
        let d2 = &s.doc().unwrap().doc;
        assert_eq!(d2.stories[&d2.item(designcraft_doc::ItemId(cap)).unwrap().text_frame().unwrap().story].text, "أمانة");
        assert!(d2.item(designcraft_doc::ItemId(card_group)).is_some());
    }

    #[test]
    fn grouped_live_caption_handles_rotated_and_nested_group_sources() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let art = s.execute("frame.create", &json!({"rect": [100, 100, 220, 200], "content": "graphic"})).unwrap()["id"].as_u64().unwrap();
        let badge = s.execute("frame.create", &json!({"rect": [90, 90, 114, 114]})).unwrap()["id"].as_u64().unwrap();
        s.execute("transform.rotate", &json!({"ids": [art], "angle": 15.0})).unwrap();
        let outer = s.execute("object.group", &json!({"ids": [badge, art]})).unwrap()["id"].as_u64().unwrap();
        s.execute("transform.move", &json!({"ids": [outer], "dx": 30.0, "dy": 15.0})).unwrap();
        s.execute("object.altText", &json!({"ids": [art], "text": "خلية نباتية"})).unwrap();

        let r =
            s.execute("object.caption", &json!({"ids": [art], "text": "{altText}", "offset": 6, "height": 22, "live": true, "group": true})).unwrap();
        let cid = designcraft_doc::ItemId(r["ids"][0].as_u64().unwrap());
        let inner_gid = designcraft_doc::ItemId(r["groups"][0].as_u64().unwrap());

        let d = &s.doc().unwrap().doc;
        let art_loc = d.find(designcraft_doc::ItemId(art)).unwrap();
        let cap_loc = d.find(cid).unwrap();
        let inner_loc = d.find(inner_gid).unwrap();
        assert_eq!(inner_loc.path.len(), 2, "inner [graphic + caption] group stays nested inside outer group");
        assert_eq!(art_loc.path.len(), 3);
        assert_eq!(cap_loc.path.len(), 3);

        let art_world = (d.parent_xf(&art_loc) * d.item(designcraft_doc::ItemId(art)).unwrap().xf)
            .transform_rect_bbox(d.item(designcraft_doc::ItemId(art)).unwrap().inner_bounds());
        let cap_world = (d.parent_xf(&cap_loc) * d.item(cid).unwrap().xf).transform_rect_bbox(d.item(cid).unwrap().inner_bounds());
        assert!((cap_world.y0 - (art_world.y1 + 6.0)).abs() < 1e-3, "caption sits 6pt below rotated graphic world bbox");
        assert!((cap_world.x0 - art_world.x0).abs() < 1e-3 && (cap_world.x1 - art_world.x1).abs() < 1e-3);
    }
}
