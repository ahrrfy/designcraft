use super::*;

pub(super) struct FittingObstacle {
    pub(super) id: ItemId,
    pub(super) bounds: Rect,
    pub(super) inherited: bool,
}

pub(super) struct BackgroundGrowth {
    pub(super) index: usize,
    pub(super) id: ItemId,
    pub(super) original: Rect,
    pub(super) target: Rect,
}

pub(super) fn is_expandable_card_shape(item: &designcraft_engine::doc::Item) -> bool {
    if !matches!(item.content, designcraft_engine::doc::Content::Unassigned) {
        return false;
    }
    match item.shape {
        designcraft_engine::doc::Shape::Rectangle => true,
        designcraft_engine::doc::Shape::Path => {
            let [sp] = item.path.subpaths.as_slice() else { return false };
            if !sp.closed || !(4..=16).contains(&sp.anchors.len()) {
                return false;
            }
            let b = item.inner_bounds();
            if !b.width().is_finite() || !b.height().is_finite() || b.width() <= 8.0 || b.height() <= 8.0 {
                return false;
            }
            let eps = 1.5;
            let padded = Rect::new(b.x0 - eps, b.y0 - eps, b.x1 + eps, b.y1 + eps);
            let on_perimeter = sp.anchors.iter().all(|a| {
                let on_edge =
                    (a.p.x - b.x0).abs() <= eps || (a.p.x - b.x1).abs() <= eps || (a.p.y - b.y0).abs() <= eps || (a.p.y - b.y1).abs() <= eps;
                on_edge && contains(padded, Rect::from_points(a.h_in, a.h_out))
            });
            let spans_top_bottom = sp.anchors.iter().any(|a| (a.p.y - b.y0).abs() <= eps) && sp.anchors.iter().any(|a| (a.p.y - b.y1).abs() <= eps);
            on_perimeter && spans_top_bottom
        }
        _ => false,
    }
}

pub(super) fn is_allowed_fit_location(doc: &designcraft_engine::doc::Document, loc: &designcraft_engine::doc::ItemLoc) -> bool {
    match loc.path.as_slice() {
        [_] => true,
        [group_idx, _] => {
            let parent_loc = designcraft_engine::doc::ItemLoc { spread: loc.spread, path: vec![*group_idx] };
            let Some(group) = doc.item_at(&parent_loc) else { return false };
            let gl = group.xf.as_coeffs();
            matches!(group.content, designcraft_engine::doc::Content::Group { .. })
                && !group.locked
                && !group.hidden
                && !group.nonprinting
                && group.overrides.is_none()
                && group.flex.is_none()
                && !group.effects.any()
                && group.wrap.is_default()
                && group.states.is_empty()
                && doc.layer(group.layer).is_none_or(|layer| !layer.locked && layer.visible && layer.printable)
                && (gl[0] - 1.0).abs() <= 0.001
                && gl[1].abs() <= 0.001
                && gl[2].abs() <= 0.001
                && (gl[3] - 1.0).abs() <= 0.001
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn is_snug_growable_background(
    obstacle: &FittingObstacle,
    text_id: ItemId,
    text_original: Rect,
    page: usize,
    doc: &designcraft_engine::doc::Document,
    base: &DocState,
    created: &BTreeMap<ItemId, usize>,
    obstacles: &[FittingObstacle],
    plan: &ProductionPlan,
    spread_index: usize,
) -> Option<usize> {
    if text_original.height() > 36.0
        || obstacle.inherited
        || obstacle.id == text_id
        || !contains(obstacle.bounds, text_original)
        || obstacle.bounds.height() > text_original.height() + 16.0
        || obstacle.bounds.width() > text_original.width() + 28.0
    {
        return None;
    }
    let &index = created.get(&obstacle.id)?;
    let item = doc.item(obstacle.id)?;
    let loc = doc.find(obstacle.id)?;
    let spread = doc.spreads.get(spread_index)?;
    let linear = (doc.parent_xf(&loc) * item.xf).as_coeffs();
    if base.doc.item(obstacle.id).is_some()
        || !is_allowed_fit_location(doc, &loc)
        || !is_expandable_card_shape(item)
        || item.locked
        || item.hidden
        || item.nonprinting
        || item.overrides.is_some()
        || item.flex.is_some()
        || item.effects.any()
        || !item.wrap.is_default()
        || doc.layer(item.layer).is_some_and(|layer| layer.locked || !layer.visible || !layer.printable)
        || (linear[0] - 1.0).abs() > 0.001
        || linear[1].abs() > 0.001
        || linear[2].abs() > 0.001
        || (linear[3] - 1.0).abs() > 0.001
        || spread.page_at_x(obstacle.bounds.center().x) != Some(page)
    {
        return None;
    }
    let single_enclosed = obstacles.iter().all(|other| {
        (!other.inherited && (other.id == obstacle.id || other.id == text_id))
            || contains(other.bounds, obstacle.bounds)
            || !overlaps(obstacle.bounds, other.bounds)
    }) && plan
        .images
        .iter()
        .filter(|img| img.page == spread_index)
        .flat_map(|img| img.all_rects())
        .all(|[x0, y0, x1, y1]| contains(Rect::new(x0, y0, x1, y1), obstacle.bounds) || !overlaps(obstacle.bounds, Rect::new(x0, y0, x1, y1)));
    single_enclosed.then_some(index)
}

/// Grow only containers whose authorship, geometry and complete new swept bands
/// are proven locally. Fixed artwork and containing images retain their bounds.
pub(super) fn containing_background_growths(
    text: &TextPosition,
    doc: &designcraft_engine::doc::Document,
    base: &DocState,
    created: &BTreeMap<ItemId, usize>,
    plan: &ProductionPlan,
    cancel: &AtomicBool,
) -> Result<Option<Vec<BackgroundGrowth>>, String> {
    let obstacles = fitting_obstacles(doc, text.spread, cancel)?;
    let mut backgrounds = Vec::new();
    let spread = doc.spreads.get(text.spread).ok_or("Native text page disappeared")?;
    let delta_top = (text.target.y0 - text.original.y0).min(0.0);
    let delta_bottom = (text.target.y1 - text.original.y1).max(0.0);
    for obstacle in &obstacles {
        if (!obstacle.inherited && obstacle.id == text.id) || !contains(obstacle.bounds, text.original) || contains(obstacle.bounds, text.target) {
            continue;
        }
        let Some(&index) = created.get(&obstacle.id) else { return Ok(None) };
        let Some(item) = doc.item(obstacle.id) else { return Ok(None) };
        let Some(loc) = doc.find(obstacle.id) else { return Ok(None) };
        let linear = (doc.parent_xf(&loc) * item.xf).as_coeffs();
        if obstacle.inherited
            || base.doc.item(obstacle.id).is_some()
            || !is_allowed_fit_location(doc, &loc)
            || !is_expandable_card_shape(item)
            || item.locked
            || item.hidden
            || item.nonprinting
            || item.overrides.is_some()
            || item.flex.is_some()
            || item.effects.any()
            || !item.wrap.is_default()
            || doc.layer(item.layer).is_some_and(|layer| layer.locked || !layer.visible || !layer.printable)
            || (linear[0] - 1.0).abs() > 0.001
            || linear[1].abs() > 0.001
            || linear[2].abs() > 0.001
            || (linear[3] - 1.0).abs() > 0.001
            || spread.page_at_x(obstacle.bounds.center().x) != Some(text.page)
        {
            return Ok(None);
        }
        if delta_top < -0.001
            && is_snug_growable_background(obstacle, text.id, text.original, text.page, doc, base, created, &obstacles, plan, text.spread).is_none()
        {
            return Ok(None);
        }
        let target = if delta_top < -0.001 {
            Rect::new(obstacle.bounds.x0, obstacle.bounds.y0 + delta_top, obstacle.bounds.x1, obstacle.bounds.y1 + delta_bottom)
        } else {
            Rect::new(obstacle.bounds.x0, obstacle.bounds.y0, obstacle.bounds.x1, obstacle.bounds.y1 + text.target.height() - text.original.height())
        };
        if ![target.x0, target.y0, target.x1, target.y1].iter().all(|value| value.is_finite())
            || !contains(text.safe, target)
            || !contains(target, text.target)
        {
            return Ok(None);
        }
        backgrounds.push(BackgroundGrowth { index, id: obstacle.id, original: obstacle.bounds, target });
    }
    let targets: BTreeMap<_, _> = backgrounds.iter().map(|background| (background.id, background.target)).collect();
    // Keep the original text-growth collision rule, substituting only containers
    // that have passed the stricter provenance and geometry checks above.
    for obstacle in &obstacles {
        if !obstacle.inherited && obstacle.id == text.id {
            continue;
        }
        let bounds = if obstacle.inherited { obstacle.bounds } else { targets.get(&obstacle.id).copied().unwrap_or(obstacle.bounds) };
        if if contains(bounds, text.original) {
            !contains(bounds, text.target)
        } else {
            overlaps(text.target, bounds) && !overlaps(text.original, bounds)
        } {
            return Ok(None);
        }
    }
    for [x0, y0, x1, y1] in plan.images.iter().filter(|image| image.page == text.spread).flat_map(|image| image.all_rects()) {
        if overlaps(text.target, Rect::new(x0, y0, x1, y1)) && !overlaps(text.original, Rect::new(x0, y0, x1, y1)) {
            return Ok(None);
        }
    }
    for background in &backgrounds {
        let mut bands = Vec::with_capacity(2);
        if background.target.y0 + 0.001 < background.original.y0 {
            bands.push(Rect::new(background.target.x0, background.target.y0, background.target.x1, background.original.y0));
        }
        if background.target.y1 > background.original.y1 + 0.001 {
            bands.push(Rect::new(background.target.x0, background.original.y1, background.target.x1, background.target.y1));
        }
        for band in bands {
            for obstacle in &obstacles {
                if cancel.load(Ordering::Relaxed) {
                    return Err("Local layout cancelled".into());
                }
                if !obstacle.inherited && obstacle.id == background.id {
                    continue;
                }
                let target = if !obstacle.inherited && obstacle.id == text.id {
                    text.target
                } else if obstacle.inherited {
                    obstacle.bounds
                } else {
                    targets.get(&obstacle.id).copied().unwrap_or(obstacle.bounds)
                };
                if background_growth_blocks(background, obstacle.bounds, target, band) {
                    return Ok(None);
                }
            }
            for [x0, y0, x1, y1] in plan.images.iter().filter(|image| image.page == text.spread).flat_map(|image| image.all_rects()) {
                let bounds = Rect::new(x0, y0, x1, y1);
                if background_growth_blocks(background, bounds, bounds, band) {
                    return Ok(None);
                }
            }
        }
    }
    Ok(Some(backgrounds))
}

pub(super) fn background_growth_blocks(background: &BackgroundGrowth, old: Rect, target: Rect, band: Rect) -> bool {
    if contains(old, background.original) {
        !contains(target, background.target)
    } else if contains(background.original, old) {
        !contains(background.target, target)
    } else {
        overlaps(band, target)
    }
}

pub(super) fn fitting_obstacles(
    doc: &designcraft_engine::doc::Document,
    spread_index: usize,
    cancel: &AtomicBool,
) -> Result<Vec<FittingObstacle>, String> {
    let spread = doc.spreads.get(spread_index).ok_or("Native text page disappeared")?;
    if spread.items.len() > 10_000 {
        return Err("Native fitting item-tree bound exceeded".into());
    }
    let mut pending: Vec<_> = spread.items.iter().map(|node| (node.as_ref(), Affine::IDENTITY, 0u32, false)).collect();
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
                pending.push((node.as_ref(), Affine::translate((page.x - parent_page.x, 0.0)), 0, true));
            }
        }
    }
    let mut obstacles = Vec::new();
    let mut visited = 0usize;
    while let Some((node, parent, depth, inherited)) = pending.pop() {
        visited = visited.saturating_add(1);
        if visited > 10_000 || depth > 64 || pending.len() > 10_000 {
            return Err("Native fitting item-tree bound exceeded".into());
        }
        if cancel.load(Ordering::Relaxed) {
            return Err("Local layout cancelled".into());
        }
        if node.children().is_empty() {
            obstacles.push(FittingObstacle { id: node.id, bounds: parent.transform_rect_bbox(node.visible_bounds()), inherited });
        } else {
            for child in node.children() {
                if pending.len() >= 10_000 {
                    return Err("Native fitting item-tree bound exceeded".into());
                }
                pending.push((child.as_ref(), parent * node.xf, depth.saturating_add(1), inherited));
            }
        }
    }
    Ok(obstacles)
}

pub(super) fn position_collision(
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
    for [x0, y0, x1, y1] in plan.images.iter().filter(|image| image.page == position.spread).flat_map(|image| image.all_rects()) {
        if blocks_translation(Rect::new(x0, y0, x1, y1), position.original, position.target) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn blocks_translation(obstacle: Rect, old: Rect, new: Rect) -> bool {
    // A containing background remains a background only while it still contains
    // the complete translated frame. Any other new intersection is a blocker.
    if contains(obstacle, old) { !contains(obstacle, new) } else { overlaps(obstacle, new) }
}

pub(super) fn shift(rect: Rect, dx: f64, dy: f64) -> Rect {
    Rect::new(rect.x0 + dx, rect.y0 + dy, rect.x1 + dx, rect.y1 + dy)
}

pub(super) fn overlaps(a: Rect, b: Rect) -> bool {
    a.x0 < b.x1 - 0.01 && a.x1 > b.x0 + 0.01 && a.y0 < b.y1 - 0.01 && a.y1 > b.y0 + 0.01
}
pub(super) fn contains(a: Rect, b: Rect) -> bool {
    a.x0 <= b.x0 + 0.01 && a.y0 <= b.y0 + 0.01 && a.x1 >= b.x1 - 0.01 && a.y1 >= b.y1 - 0.01
}
