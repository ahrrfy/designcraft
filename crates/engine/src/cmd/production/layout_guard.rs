//! Draft layout geometry, margin, overlap, and opaque-occluder preflight guards.

use serde_json::{Value, json};

use super::super::bad;
use super::{ID, MAX_IMAGE_PIXELS};
use crate::{Result, Session};

/// A rendered document-page instance. A parent frame can have an instance on several pages.
struct LayoutFrame<'a> {
    item: &'a designcraft_doc::Item,
    loc: designcraft_doc::ItemLoc,
    story: &'a std::sync::Arc<designcraft_doc::Story>,
    parent: designcraft_geom::Affine,
    bounds: designcraft_geom::Rect,
    safe: designcraft_geom::Rect,
    spread_index: usize,
    paint_order: usize,
    page: usize,
    page_id: u64,
}

/// An opaque non-text object rendered on a document spread in paint order.
struct OpaqueOccluder<'a> {
    item: &'a designcraft_doc::Item,
    loc: designcraft_doc::ItemLoc,
    parent: designcraft_geom::Affine,
    bounds: designcraft_geom::Rect,
    spread_index: usize,
    forced_page: Option<usize>,
    paint_order: usize,
    has_opaque_fill: bool,
    eff_opacity: f32,
    asset_ptr: Option<*const Vec<u8>>,
}

enum GraphicAlpha {
    FullyOpaque,
    Raster { alpha: Vec<u8>, width: u32, height: u32 },
    TransparentOrUnknown,
}

fn summarize_alpha_plane(alpha: Vec<u8>, width: u32, height: u32) -> GraphicAlpha {
    if alpha.is_empty() || alpha.iter().all(|&a| a < 230) {
        GraphicAlpha::TransparentOrUnknown
    } else if alpha.iter().all(|&a| a == 255) {
        GraphicAlpha::FullyOpaque
    } else {
        GraphicAlpha::Raster { alpha, width, height }
    }
}

fn classify_graphic_alpha(asset: &designcraft_doc::Asset) -> GraphicAlpha {
    if designcraft_doc::media_kind(&asset.mime).is_some() {
        return GraphicAlpha::FullyOpaque;
    }
    let bytes = asset.data.as_slice();
    if bytes.is_empty() || designcraft_images::is_pdf(bytes) {
        return GraphicAlpha::TransparentOrUnknown;
    }
    if asset.mime.eq_ignore_ascii_case("image/jpeg") || bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return GraphicAlpha::FullyOpaque;
    }
    if designcraft_images::is_svg(bytes) {
        let Some((rgba, width, height)) = designcraft_images::render_svg(bytes, 256) else {
            return GraphicAlpha::TransparentOrUnknown;
        };
        if width == 0 || height == 0 {
            return GraphicAlpha::TransparentOrUnknown;
        }
        let alpha: Vec<u8> = rgba.chunks_exact(4).map(|px| px.get(3).copied().unwrap_or(0)).collect();
        return summarize_alpha_plane(alpha, width, height);
    }
    let Some((w, h)) = designcraft_images::pixel_size(bytes) else {
        return GraphicAlpha::TransparentOrUnknown;
    };
    if w == 0 || h == 0 || u64::from(w).saturating_mul(u64::from(h)) > MAX_IMAGE_PIXELS {
        return GraphicAlpha::TransparentOrUnknown;
    }
    let Some(rgba) = designcraft_images::decode_rgba(bytes) else {
        return GraphicAlpha::TransparentOrUnknown;
    };
    let (width, height) = rgba.dimensions();
    if width == 0 || height == 0 {
        return GraphicAlpha::TransparentOrUnknown;
    }
    let alpha: Vec<u8> = rgba.pixels().map(|px| px.0[3]).collect();
    summarize_alpha_plane(alpha, width, height)
}

fn graphic_opaque_area_in_rect(
    doc: &designcraft_doc::Document,
    occ: &OpaqueOccluder<'_>,
    inter: designcraft_geom::Rect,
    alpha_cache: &mut std::collections::HashMap<designcraft_doc::AssetId, GraphicAlpha>,
) -> f64 {
    use designcraft_geom::{Point, Shape as _};
    let area = inter.width().max(0.0) * inter.height().max(0.0);
    if area <= 0.0 {
        return 0.0;
    }
    let Some(g) = occ.item.drawn_graphic(&doc.assets) else {
        return 0.0;
    };
    if !(g.size.0.is_finite() && g.size.1.is_finite() && g.size.0 > 0.0 && g.size.1 > 0.0) {
        return 0.0;
    }
    let Some(asset) = doc.assets.get(&g.asset) else {
        return 0.0;
    };
    let alpha_info = alpha_cache.entry(g.asset).or_insert_with(|| classify_graphic_alpha(asset));
    if matches!(alpha_info, GraphicAlpha::TransparentOrUnknown) {
        return 0.0;
    }
    let frame_xf = occ.parent * occ.item.xf;
    let frame_det = frame_xf.determinant();
    let g_det = g.xf.determinant();
    if !(frame_det.is_finite() && frame_det.abs() > 1e-9 && g_det.is_finite() && g_det.abs() > 1e-9) {
        return 0.0;
    }
    let fc = frame_xf.as_coeffs();
    let gc = g.xf.as_coeffs();
    let axis_aligned_rect = occ.item.shape == designcraft_doc::Shape::Rectangle
        && g.clip.is_none()
        && fc[1].abs() <= 1e-6
        && fc[2].abs() <= 1e-6
        && gc[1].abs() <= 1e-6
        && gc[2].abs() <= 1e-6;
    if matches!(alpha_info, GraphicAlpha::FullyOpaque) && axis_aligned_rect {
        return area;
    }
    let to_frame = frame_xf.inverse();
    let to_graphic = g.xf.inverse();
    let inner = occ.item.inner_bounds();
    let frame_bez = (occ.item.shape != designcraft_doc::Shape::Rectangle).then(|| occ.item.path.to_bezpath());
    let clip = g.clip_region();
    const GRID: usize = 16;
    let mut opaque_hits = 0usize;
    for gy in 0..GRID {
        let sy = inter.y0 + ((gy as f64 + 0.5) / GRID as f64) * inter.height();
        for gx in 0..GRID {
            let sx = inter.x0 + ((gx as f64 + 0.5) / GRID as f64) * inter.width();
            let p_frame = to_frame * Point::new(sx, sy);
            if !inner.contains(p_frame) {
                continue;
            }
            if let Some(bez) = &frame_bez
                && bez.winding(p_frame) == 0
            {
                continue;
            }
            if let Some((clip_path, even_odd)) = &clip {
                let w = clip_path.winding(p_frame);
                let inside = if *even_odd { w % 2 != 0 } else { w != 0 };
                if !inside {
                    continue;
                }
            }
            let p_graphic = to_graphic * p_frame;
            if !(p_graphic.x >= 0.0 && p_graphic.x < g.size.0 && p_graphic.y >= 0.0 && p_graphic.y < g.size.1) {
                continue;
            }
            let a = match alpha_info {
                GraphicAlpha::FullyOpaque => 255u8,
                GraphicAlpha::Raster { alpha, width, height } => {
                    let px = ((p_graphic.x / g.size.0) * f64::from(*width)).floor().clamp(0.0, f64::from(width.saturating_sub(1))) as usize;
                    let py = ((p_graphic.y / g.size.1) * f64::from(*height)).floor().clamp(0.0, f64::from(height.saturating_sub(1))) as usize;
                    alpha.get(py.saturating_mul(*width as usize).saturating_add(px)).copied().unwrap_or(0)
                }
                GraphicAlpha::TransparentOrUnknown => 0u8,
            };
            if (f32::from(a) / 255.0) * occ.eff_opacity >= 0.9 {
                opaque_hits = opaque_hits.saturating_add(1);
            }
        }
    }
    area * (opaque_hits as f64 / (GRID * GRID) as f64)
}

fn layout_scene(s: &Session) -> Result<(Vec<LayoutFrame<'_>>, Vec<OpaqueOccluder<'_>>)> {
    use designcraft_doc::{ItemLoc, Shape, SpreadRef};
    use designcraft_geom::Affine;
    let d = &s.doc()?.doc;
    // Validate the complete tree before any location lookup; imported hidden groups count too.
    if d.spreads.len() > 1000 || d.parents.len() > 1024 {
        return Err(bad(ID, "textLayoutLimit: too many document or parent spreads"));
    }
    let mut tree: Vec<_> =
        d.spread_refs().filter_map(|r| d.spread(r)).flat_map(|sp| sp.items.iter().map(|it| (it.as_ref(), 0usize))).take(65537).collect();
    if tree.len() > 65536 {
        return Err(bad(ID, "textLayoutLimit: more than 65536 document items"));
    }
    let mut count = 0usize;
    while let Some((item, depth)) = tree.pop() {
        count = count.saturating_add(1);
        if count > 65536 || depth > 64 {
            return Err(bad(ID, "textLayoutLimit: document exceeds 65536 items or 64 group levels"));
        }
        let remaining = 65537usize.saturating_sub(tree.len());
        tree.extend(item.children().iter().take(remaining).map(|child| (child.as_ref(), depth + 1)));
        if tree.len() > 65536 {
            return Err(bad(ID, "textLayoutLimit: more than 65536 pending document items"));
        }
    }
    // Walk each document spread in exact Renderer::draw_spread back-to-front paint order:
    // layers back-to-front (`d.layers.iter().rev()`), inherited parent items first, then spread items,
    // and group children in forward `0..children.len()` order (via `.rev()` on a LIFO stack).
    let mut frames = Vec::new();
    let mut occluders = Vec::new();
    let mut visited = 0usize;
    let known_layers: std::collections::HashSet<_> = d.layers.iter().map(|l| l.id).collect();

    for (si, sp) in d.spreads.iter().enumerate() {
        let sr = SpreadRef::Doc(si);
        let mut spread_roots = Vec::new();
        for layer in d.layers.iter().rev() {
            let layer_vis = layer.visible && layer.printable;
            for (pi, page) in sp.pages.iter().enumerate().filter(|(_, page)| page.show_parent_items) {
                for (ppi, ppage) in d.shown_parent_pages(sr, pi) {
                    let Some(parent) = d.parents.get(ppi) else { continue };
                    let Some(ppg) = parent.pages.get(ppage) else { continue };
                    for (index, item) in parent.items.iter().enumerate() {
                        if item.layer != layer.id
                            || page.overridden.contains(&item.id)
                            || (parent.pages.len() > 1 && parent.page_at_x(item.bounds().center().x) != Some(ppage))
                        {
                            continue;
                        }
                        if spread_roots.len() >= 65536 {
                            return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
                        }
                        spread_roots.push((
                            ItemLoc { spread: SpreadRef::Parent(ppi), path: vec![index] },
                            Affine::translate((page.x - ppg.x, 0.0)),
                            Some(d.first_page_of_spread(si) + pi),
                            layer_vis,
                            true,
                            1.0_f32,
                            true,
                        ));
                    }
                }
            }
            for (index, item) in sp.items.iter().enumerate() {
                if item.layer != layer.id {
                    continue;
                }
                if spread_roots.len() >= 65536 {
                    return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
                }
                spread_roots.push((ItemLoc { spread: sr, path: vec![index] }, Affine::IDENTITY, None, layer_vis, true, 1.0_f32, true));
            }
        }
        // Include any items whose layer id is missing from `d.layers` (as non-visible) so instance bounds remain consistent.
        for (index, item) in sp.items.iter().enumerate() {
            if !known_layers.contains(&item.layer) {
                if spread_roots.len() >= 65536 {
                    return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
                }
                spread_roots.push((ItemLoc { spread: sr, path: vec![index] }, Affine::IDENTITY, None, false, true, 1.0_f32, true));
            }
        }

        let mut pending: Vec<_> = spread_roots.into_iter().rev().collect();
        let mut next_paint_order = 0usize;
        while let Some((loc, placement, forced_page, inherited_vis, active_state, inherited_opacity, inherited_normal_blend)) = pending.pop() {
            visited = visited.saturating_add(1);
            if visited > 65536 || pending.len() > 65536 {
                return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
            }
            let item = d.item_at(&loc).ok_or_else(|| bad(ID, "textLayoutLocation: invalid item location"))?;
            let item_layer_vis = d.layer(item.layer).is_some_and(|layer| layer.visible && layer.printable);
            let visible = inherited_vis && item_layer_vis && !item.hidden && !item.nonprinting && item.opacity.is_finite() && item.opacity > 0.0;
            let item_opacity = if item.opacity.is_finite() { item.opacity.clamp(0.0, 1.0) } else { 0.0 };
            let eff_opacity = inherited_opacity * item_opacity;
            let eff_normal_blend = inherited_normal_blend && item.blend == designcraft_color::BlendMode::Normal;
            let paint_order = next_paint_order;
            next_paint_order = next_paint_order.saturating_add(1);

            let only_state = (!item.states.is_empty()).then_some(item.active_state);
            for (index, _) in item.children().iter().enumerate().rev() {
                if pending.len() >= 65536 {
                    return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
                }
                let mut child = loc.clone();
                child.path.push(index);
                let child_active = active_state && only_state.is_none_or(|st| st == index);
                pending.push((child, placement, forced_page, visible, child_active, eff_opacity, eff_normal_blend));
            }
            if !visible {
                continue;
            }
            let parent = placement * d.parent_xf(&loc);
            if let Some(tf) = item.text_frame() {
                let story = d.stories.get(&tf.story).ok_or_else(|| bad(ID, format!("textLayoutStory: item {} has no story", item.id.0)))?;
                let composed = s.cache.get(d, tf.story, None);
                let nonempty =
                    composed.frame(item.id).is_some_and(|frame| story.text.get(frame.range.clone()).is_some_and(|text| !text.trim().is_empty()));
                if !nonempty {
                    continue;
                }
                let bounds = parent.transform_rect_bbox(item.bounds());
                if ![bounds.x0, bounds.y0, bounds.x1, bounds.y1].iter().all(|v| v.is_finite()) {
                    return Err(bad(ID, format!("textLayoutGeometry: item {} has nonfinite bounds", item.id.0)));
                }
                let page = if let Some(page) = forced_page {
                    page
                } else {
                    let SpreadRef::Doc(doc_si) = loc.spread else {
                        return Err(bad(ID, "textLayoutLocation: parent item without an owning page"));
                    };
                    d.first_page_of_spread(doc_si)
                        + d.spread(loc.spread)
                            .and_then(|s_sp| s_sp.page_at_x(bounds.center().x))
                            .ok_or_else(|| bad(ID, format!("textOutsideMargins: item {} has no owning page", item.id.0)))?
                };
                let owning = d.page(page).ok_or_else(|| bad(ID, "textLayoutLocation: no owning page"))?;
                frames.push(LayoutFrame {
                    item,
                    loc,
                    story,
                    parent,
                    bounds,
                    safe: owning.margin_rect(),
                    spread_index: si,
                    paint_order,
                    page,
                    page_id: owning.id.0,
                });
                if frames.len() > 4096 {
                    return Err(bad(ID, "textLayoutLimit: more than 4096 nonempty printable text-frame instances"));
                }
            } else if active_state
                && !item.is_group()
                && item.shape != Shape::GraphicLine
                && (item.shape != Shape::Path || item.path.subpaths.iter().any(|sub| sub.closed))
                && eff_opacity >= 0.9
                && eff_normal_blend
            {
                let has_opaque_fill = !item.fill.is_none() && item.fill.tint.is_finite() && item.fill.tint >= 0.9;
                let drawn_graphic = (!has_opaque_fill).then(|| item.drawn_graphic(&d.assets)).flatten();
                let asset_ptr = drawn_graphic
                    .as_ref()
                    .and_then(|g| d.assets.get(&g.asset))
                    .filter(|a| !a.data.is_empty() || designcraft_doc::media_kind(&a.mime).is_some())
                    .map(|a| std::sync::Arc::as_ptr(&a.data));
                let occ_bounds = if has_opaque_fill {
                    Some(parent.transform_rect_bbox(item.bounds()))
                } else if let Some(g) = &drawn_graphic {
                    asset_ptr.and_then(|_| g.shown_bounds(item.inner_bounds()).map(|r| (parent * item.xf).transform_rect_bbox(r)))
                } else {
                    None
                };
                if let Some(bounds) = occ_bounds.filter(|b| [b.x0, b.y0, b.x1, b.y1].iter().all(|v| v.is_finite()))
                    && bounds.width() > 0.0
                    && bounds.height() > 0.0
                {
                    occluders.push(OpaqueOccluder {
                        item,
                        loc,
                        parent,
                        bounds,
                        spread_index: si,
                        forced_page,
                        paint_order,
                        has_opaque_fill,
                        eff_opacity,
                        asset_ptr,
                    });
                }
            }
        }
    }
    Ok((frames, occluders))
}

pub(super) fn check_text_layout(original: &Session, draft: &Session) -> Result<()> {
    const TOLERANCE: f64 = 0.25;
    const MIN_OCCLUSION_DIM_PT: f64 = 4.0;
    const MIN_OCCLUSION_RATIO: f64 = 0.35;
    let (before, before_occluders) = layout_scene(original)?;
    let (after, after_occluders) = layout_scene(draft)?;
    let previous: std::collections::HashMap<_, _> = before.iter().map(|frame| ((frame.item.id, frame.page_id), frame)).collect();
    let previous_occluders: std::collections::HashMap<_, _> =
        before_occluders.iter().map(|occ| ((occ.item.id, occ.spread_index, occ.forced_page), occ)).collect();
    let changed: Vec<_> = after
        .iter()
        .map(|frame| {
            previous.get(&(frame.item.id, frame.page_id)).is_none_or(|old| {
                old.item != frame.item
                    || !std::sync::Arc::ptr_eq(old.story, frame.story)
                    || old.parent != frame.parent
                    || old.safe != frame.safe
                    || old.bounds != frame.bounds
            })
        })
        .collect();
    let occluder_changed: Vec<_> = after_occluders
        .iter()
        .map(|occ| {
            previous_occluders.get(&(occ.item.id, occ.spread_index, occ.forced_page)).is_none_or(|old| {
                old.item != occ.item
                    || old.parent != occ.parent
                    || old.bounds != occ.bounds
                    || old.has_opaque_fill != occ.has_opaque_fill
                    || (old.eff_opacity - occ.eff_opacity).abs() > 1e-4
                    || old.asset_ptr != occ.asset_ptr
            })
        })
        .collect();
    let mut issues = Vec::new();
    for (i, frame) in after.iter().enumerate().filter(|(i, _)| changed.get(*i) == Some(&true)) {
        let b = frame.bounds;
        let m = frame.safe;
        if b.x0 < m.x0 - TOLERANCE || b.y0 < m.y0 - TOLERANCE || b.x1 > m.x1 + TOLERANCE || b.y1 > m.y1 + TOLERANCE {
            issues.push(json!({"kind":"textOutsideMargins","item":frame.item.id.0,"page":frame.page+1,
                "bounds":[b.x0,b.y0,b.x1,b.y1],"safeMargins":[m.x0,m.y0,m.x1,m.y1]}));
        }
        for (j, other) in after.iter().enumerate() {
            if i == j || frame.page_id != other.page_id || (changed.get(j) == Some(&true) && j < i) {
                continue;
            }
            let o = other.bounds;
            if b.x1.min(o.x1) - b.x0.max(o.x0) > TOLERANCE && b.y1.min(o.y1) - b.y0.max(o.y0) > TOLERANCE {
                issues.push(json!({"kind":"textOverlap","item":frame.item.id.0,"otherItem":other.item.id.0,"page":frame.page+1,
                    "bounds":[b.x0,b.y0,b.x1,b.y1],"otherBounds":[o.x0,o.y0,o.x1,o.y1]}));
            }
            if issues.len() >= 32 {
                break;
            }
        }
        if issues.len() >= 32 {
            break;
        }
    }
    let draft_doc = &draft.doc()?.doc;
    let mut alpha_cache = std::collections::HashMap::new();
    if issues.len() < 32 {
        for (i, frame) in after.iter().enumerate() {
            let b = frame.bounds;
            let frame_area = b.width().max(0.0) * b.height().max(0.0);
            if frame_area <= TOLERANCE {
                continue;
            }
            let frame_is_changed = changed.get(i) == Some(&true);
            let old_frame = previous.get(&(frame.item.id, frame.page_id)).copied();
            for (k, occ) in after_occluders.iter().enumerate() {
                if occ.spread_index != frame.spread_index || occ.paint_order <= frame.paint_order {
                    continue;
                }
                // Container frames with pasted-into children draw their fill behind nested children.
                if occ.loc.spread == frame.loc.spread && frame.loc.path.starts_with(&occ.loc.path) {
                    continue;
                }
                let occ_is_changed = occluder_changed.get(k) == Some(&true);
                let preexisting_above = !frame_is_changed
                    && !occ_is_changed
                    && old_frame
                        .zip(previous_occluders.get(&(occ.item.id, occ.spread_index, occ.forced_page)).copied())
                        .is_some_and(|(prev_f, prev_o)| prev_o.paint_order > prev_f.paint_order);
                if preexisting_above {
                    continue;
                }
                let o = occ.bounds;
                let inter = designcraft_geom::Rect::new(b.x0.max(o.x0), b.y0.max(o.y0), b.x1.min(o.x1), b.y1.min(o.y1));
                let ix = inter.width().max(0.0);
                let iy = inter.height().max(0.0);
                if ix > MIN_OCCLUSION_DIM_PT && iy > MIN_OCCLUSION_DIM_PT && ix * iy >= MIN_OCCLUSION_RATIO * frame_area {
                    let opaque_area =
                        if occ.has_opaque_fill { ix * iy } else { graphic_opaque_area_in_rect(draft_doc, occ, inter, &mut alpha_cache) };
                    if opaque_area >= MIN_OCCLUSION_RATIO * frame_area {
                        issues.push(json!({
                            "kind": "textOccludedByObject",
                            "item": frame.item.id.0,
                            "occludingItem": occ.item.id.0,
                            "page": frame.page + 1,
                            "bounds": [b.x0, b.y0, b.x1, b.y1],
                            "occluderBounds": [o.x0, o.y0, o.x1, o.y1],
                        }));
                        if issues.len() >= 32 {
                            break;
                        }
                    }
                }
            }
            if issues.len() >= 32 {
                break;
            }
        }
    }
    if !issues.is_empty() {
        return Err(bad(ID, format!("text layout rejected draft: {}", Value::Array(issues))));
    }
    Ok(())
}
