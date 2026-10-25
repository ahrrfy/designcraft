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
    let mut notes = normalize_command_aliases(&mut candidate);
    notes.extend(format_populated_new_stories(&mut candidate)?);
    notes.extend(recover_unregistered_swatches(&mut candidate, base)?);
    notes.extend(normalize_unit_interval_fields(&mut candidate));
    crate::image_repetition::normalize(&mut candidate)?;
    crate::design::check_document_image_slots(base, &candidate.images)?;
    let mut scratch = Session::new();
    scratch.prefs = prefs.clone();
    scratch.add_document(base.clone());
    let mut pre_existing_overset = BTreeSet::new();
    for &story_id in base.doc.stories.keys() {
        if scratch.cache.get(&base.doc, story_id, None).is_overset() {
            pre_existing_overset.insert(story_id);
        }
    }
    let result = scratch
        .execute("production.apply", &json!({"commands":candidate.commands,"outputMode":"native_with_images","requirePreflight":false}))
        .map_err(|e| e.to_string())?;
    let results = result.get("results").and_then(Value::as_array).ok_or("Native layout did not return command results")?;
    let mut frames = Vec::new();
    let mut frame_refs: BTreeMap<ItemId, (usize, String, String)> = BTreeMap::new();
    let mut created_frames = BTreeMap::new();
    let current_doc = &scratch.doc().map_err(|e| e.to_string())?.doc;
    for (index, action) in candidate.commands.iter().enumerate() {
        if ((action.command == "frame.create"
            && matches!(action.params.get("content").and_then(Value::as_str), None | Some("unassigned"))
            && action.params.get("text").is_none())
            || (action.command == "path.create" && action.params.get("closed").and_then(Value::as_bool) == Some(true)))
            && let Some(id) = results.get(index).and_then(|value| value.get("id")).and_then(Value::as_u64)
            && current_doc.item(ItemId(id)).is_some_and(is_expandable_card_shape)
        {
            created_frames.insert(ItemId(id), index);
        }
        if action.command == "frame.create" && action.params.get("content").and_then(Value::as_str) == Some("text") {
            let id = results.get(index).and_then(|v| v.get("id")).and_then(Value::as_u64).ok_or("Native text frame has no ID")?;
            let initial_story = results.get(index).and_then(|v| v.get("story")).and_then(Value::as_u64).ok_or("Native text frame has no story")?;
            let Some(item) = current_doc.item(ItemId(id)) else {
                continue;
            };
            let story_id = item.text_frame().map(|tf| tf.story).unwrap_or(StoryId(initial_story));
            if current_doc.story(story_id).is_some() {
                frames.push((index, ItemId(id), story_id));
                frame_refs.insert(ItemId(id), (index, format!("${index}.id"), format!("${index}.story")));
            }
        }
        if action.command == "text.thread" && action.params.get("rect").is_some() {
            let field = if action.params.get("from").is_some() { "to" } else { "from" };
            if let Some(id) = results.get(index).and_then(|v| v.get(field)).and_then(Value::as_u64)
                && let Some(initial_story) = results.get(index).and_then(|v| v.get("story")).and_then(Value::as_u64)
                && let Some(item) = current_doc.item(ItemId(id))
            {
                let story_id = item.text_frame().map(|tf| tf.story).unwrap_or(StoryId(initial_story));
                if current_doc.story(story_id).is_some() {
                    frames.push((index, ItemId(id), story_id));
                    frame_refs.insert(ItemId(id), (index, format!("${index}.{field}"), format!("${index}.story")));
                }
            }
        }
    }
    notes.extend(recover_text_positions(&mut candidate, &mut scratch, base, &frames, &frame_refs, cancel)?);
    let mut seen = BTreeSet::new();
    for &(index, id, story_id) in &frames {
        if cancel.load(Ordering::Relaxed) {
            return Err("Local layout cancelled".into());
        }
        if !seen.insert(story_id) {
            continue;
        }
        let story_ref = frame_refs.get(&id).map(|(_, _, sr)| Value::String(sr.clone())).unwrap_or_else(|| Value::String(format!("${index}.story")));
        apply_story_glyph_fallbacks(story_id, id, &story_ref, &mut scratch, &mut candidate, &mut notes)?;
        let doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
        let story = doc.story(story_id).ok_or("Native layout story disappeared")?;
        if !scratch.cache.get(&doc, story_id, None).is_overset() {
            continue;
        }
        let target_id = doc.story(story_id).and_then(|st| st.frames.last().copied()).unwrap_or(id);
        let (fit_index, fit_id, fit_is_existing_base) = if let Some((idx, _, _)) = frame_refs.get(&target_id) {
            (*idx, target_id, false)
        } else if base.doc.item(target_id).is_some() {
            (index, target_id, true)
        } else {
            (index, id, base.doc.item(id).is_some())
        };
        let fit_item_ref = frame_refs.get(&fit_id).map(|(_, r, _)| Value::String(r.clone())).unwrap_or_else(|| json!(fit_id.0));
        let loc = doc.find(fit_id).ok_or("Native text frame location disappeared")?;
        let designcraft_engine::doc::SpreadRef::Doc(spread_index) = loc.spread else {
            return Err("Cannot fit a parent-page frame in production".into());
        };
        let spread = doc.spreads.get(spread_index).ok_or("Native page disappeared")?;
        let item = doc.item(fit_id).ok_or("Native frame disappeared")?;
        let outer = doc.parent_xf(&loc);
        let old = outer.transform_rect_bbox(item.bounds());
        let identity = frame_identity(spread_index, fit_index, fit_id, old, &story.text);
        let linear = (outer * item.xf).as_coeffs();
        if (linear[0] - 1.0).abs() > 0.001 || linear[1].abs() > 0.001 || linear[2].abs() > 0.001 || (linear[3] - 1.0).abs() > 0.001 {
            return Err(format!("{identity} has overset text in a transformed frame; reflow its layout before generation"));
        }
        let pre_fit_state = scratch.doc().map_err(|e| e.to_string())?.clone();
        scratch.execute("object.fit", &json!({"ids":[fit_id.0],"mode":"fitFrameToContent","axis":"height"})).map_err(|e| e.to_string())?;
        let fitted_doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
        let fitted = fitted_doc.item(fit_id).ok_or("Fitted text frame disappeared")?;
        let measured = outer.transform_rect_bbox(fitted.bounds());
        let story_has_arabic_marks =
            story.text.chars().any(|c| ('\u{064B}'..='\u{065F}').contains(&c) || c == '\u{0670}' || ('\u{06D6}'..='\u{06ED}').contains(&c));
        let page = spread.page_at_x(old.center().x).ok_or("Native text frame has no owning page")?;
        let safe = spread.pages.get(page).ok_or("Native text page disappeared")?.margin_rect();
        let mark_pad =
            if story_has_arabic_marks && contains(safe, Rect::new(measured.x0, measured.y0, measured.x1, measured.y1 + 2.0)) { 2.0 } else { 0.01 };
        let extra_height = measured.height() - old.height() + mark_pad;
        // Include the same rounding/mark allowance in collision checks and the published source.
        let grown = Rect::new(measured.x0, measured.y0, measured.x1, measured.y1 + mark_pad);
        let allowed_loc = is_allowed_fit_location(&doc, &loc);
        let mut growth = TextPosition { index: fit_index, id: fit_id, spread: spread_index, page, original: old, target: grown, safe, language: 0 };
        let mut blocked = !extra_height.is_finite()
            || extra_height <= 0.0
            || !allowed_loc
            || (fit_is_existing_base
                && (loc.path.len() != 1
                    || item.locked
                    || item.hidden
                    || item.nonprinting
                    || doc.layer(item.layer).is_some_and(|layer| layer.locked || !layer.visible || !layer.printable)));
        // Height fitting happens after ordinary margin recovery. A new footer can
        // retain its bottom edge by moving only the measured excess upward.
        if !contains(safe, grown) {
            growth.target = shift(grown, 0.0, safe.y1 - grown.y1);
            blocked |= grown.y1 <= safe.y1
                || !allowed_loc
                || fit_is_existing_base
                || item.locked
                || item.hidden
                || item.nonprinting
                || doc.layer(item.layer).is_some_and(|layer| layer.locked || !layer.visible || !layer.printable)
                || !contains(safe, growth.target)
                || position_collision(&growth, &BTreeMap::from([(fit_id, growth.target)]), &doc, &candidate, cancel)?;
        }
        let mut backgrounds = if blocked { None } else { containing_background_growths(&growth, &doc, base, &created_frames, &candidate, cancel)? };
        if backgrounds.is_none()
            && extra_height.is_finite()
            && extra_height > 0.0
            && grown.y1 > old.y1
            && !fit_is_existing_base
            && allowed_loc
            && !item.locked
            && !item.hidden
            && !item.nonprinting
            && !doc.layer(item.layer).is_some_and(|layer| layer.locked || !layer.visible || !layer.printable)
        {
            let all_obstacles = fitting_obstacles(&doc, spread_index, cancel)?;
            let snug_pills: BTreeMap<ItemId, (usize, Rect)> = all_obstacles
                .iter()
                .filter_map(|obs| {
                    let idx =
                        is_snug_growable_background(obs, fit_id, old, page, &doc, base, &created_frames, &all_obstacles, &candidate, spread_index)?;
                    Some((obs.id, (idx, obs.bounds)))
                })
                .collect();
            let mut enclosing_containers: Vec<(Rect, Option<f64>)> = all_obstacles
                .iter()
                .filter(|obs| (obs.inherited || obs.id != fit_id) && !snug_pills.contains_key(&obs.id) && contains(obs.bounds, old))
                .map(|obs| {
                    let panel_min_y0 = if !obs.inherited
                        && created_frames.contains_key(&obs.id)
                        && base.doc.item(obs.id).is_none()
                        && obs.bounds.y1 >= grown.y1 + 16.0
                        && let Some(it) = doc.item(obs.id)
                        && it.shape == designcraft_engine::doc::Shape::Rectangle
                        && matches!(it.content, designcraft_engine::doc::Content::Unassigned)
                    {
                        let inner_stroke_y0 = doc
                            .find(obs.id)
                            .map(|l| doc.parent_xf(&l).transform_rect_bbox(it.bounds()).y0 + 0.5 * it.stroke.weight.max(0.0))
                            .unwrap_or(obs.bounds.y0);
                        Some(obs.bounds.y0.max(inner_stroke_y0) + 0.5)
                    } else {
                        None
                    };
                    (obs.bounds, panel_min_y0)
                })
                .collect();
            enclosing_containers.extend(
                candidate
                    .images
                    .iter()
                    .filter(|img| img.page == spread_index)
                    .flat_map(|img| img.all_rects())
                    .map(|[x0, y0, x1, y1]| Rect::new(x0, y0, x1, y1))
                    .filter(|&bounds| contains(bounds, old))
                    .map(|bounds| (bounds, None)),
            );
            let compact_frame = old.height() <= 36.0;
            let container_ok = |candidate_rect: Rect| {
                enclosing_containers.iter().all(|&(c, panel_min_y0)| {
                    if compact_frame {
                        let top_pad = (old.y0 - c.y0).clamp(0.0, 0.5);
                        let bot_pad = (c.y1 - old.y1).clamp(0.0, 0.5);
                        candidate_rect.y0 >= c.y0 + top_pad && candidate_rect.y1 <= c.y1 - bot_pad && contains(c, candidate_rect)
                    } else if let Some(min_y0) = panel_min_y0 {
                        let bot_pad = (c.y1 - old.y1).clamp(0.0, 0.5);
                        candidate_rect.y0 >= min_y0 && candidate_rect.y1 <= c.y1 - bot_pad && contains(c, candidate_rect)
                    } else {
                        candidate_rect.y0 >= c.y0 + 8.0 && candidate_rect.y1 <= c.y1 - 0.5 && contains(c, candidate_rect)
                    }
                })
            };
            let full_up_dy = old.y1 - grown.y1;
            let pill_bot_pad = snug_pills.values().map(|(_, r)| (r.y1 - old.y1).max(0.0)).fold(0.0_f64, f64::max);
            let min_allowed_dy = full_up_dy - pill_bot_pad - 0.5;
            let mut shift_candidates: Vec<f64> =
                [1.0_f64, 0.85, 0.75, 0.65, 0.5, 0.35, 0.25, 0.15].into_iter().map(|frac| full_up_dy * frac).collect();
            let safe_ceil_dy = safe.y0 - grown.y0;
            if safe_ceil_dy < 0.0 && safe_ceil_dy > full_up_dy {
                shift_candidates.push(safe_ceil_dy);
            }
            if !enclosing_containers.is_empty() {
                let min_c_y1 = enclosing_containers.iter().map(|&(c, _)| c.y1 - (c.y1 - old.y1).clamp(0.0, 0.5)).fold(f64::INFINITY, f64::min);
                if min_c_y1.is_finite() {
                    shift_candidates.push(min_c_y1 - grown.y1);
                }
                for &(c, panel_min_y0) in &enclosing_containers {
                    if compact_frame {
                        let top_pad = (old.y0 - c.y0).clamp(0.0, 0.5);
                        let c_ceil_dy = (c.y0 + top_pad) - grown.y0;
                        if c_ceil_dy < 0.0 && c_ceil_dy > full_up_dy {
                            shift_candidates.push(c_ceil_dy);
                        }
                    } else if let Some(min_y0) = panel_min_y0 {
                        let c_ceil_dy = min_y0 - grown.y0;
                        if c_ceil_dy < 0.0 && c_ceil_dy > full_up_dy {
                            shift_candidates.push(c_ceil_dy);
                        }
                    }
                }
            }
            let x_min = snug_pills.values().map(|(_, r)| r.x0).fold(old.x0, f64::min);
            let x_max = snug_pills.values().map(|(_, r)| r.x1).fold(old.x1, f64::max);
            for obs_bounds in all_obstacles
                .iter()
                .filter(|obs| (obs.inherited || (obs.id != fit_id && !snug_pills.contains_key(&obs.id))) && !contains(obs.bounds, old))
                .map(|obs| obs.bounds)
                .chain(
                    candidate
                        .images
                        .iter()
                        .filter(|img| img.page == spread_index)
                        .flat_map(|img| img.all_rects())
                        .map(|[x0, y0, x1, y1]| Rect::new(x0, y0, x1, y1))
                        .filter(|&b| !contains(b, old)),
                )
            {
                if obs_bounds.x0 < x_max && obs_bounds.x1 > x_min && obs_bounds.y0 >= old.y1 - 0.05 && obs_bounds.y0 <= grown.y1 + 16.0 {
                    shift_candidates.push((obs_bounds.y0 - 0.02) - grown.y1);
                    if pill_bot_pad > 0.0 {
                        shift_candidates.push((obs_bounds.y0 - pill_bot_pad - 0.02) - grown.y1);
                    }
                }
            }
            let max_container_extra =
                if compact_frame || enclosing_containers.iter().all(|&(_, panel_min_y0)| panel_min_y0.is_some()) { 16.0 } else { 12.0 };
            for dy_candidate in shift_candidates {
                if !dy_candidate.is_finite() || dy_candidate > 0.0 || dy_candidate < min_allowed_dy {
                    continue;
                }
                let shifted = shift(grown, 0.0, dy_candidate);
                let container_safe = enclosing_containers.is_empty() || (extra_height <= max_container_extra && container_ok(shifted));
                if !container_safe || !contains(safe, shifted) {
                    continue;
                }
                let upward_growth =
                    TextPosition { index: fit_index, id: fit_id, spread: spread_index, page, original: old, target: shifted, safe, language: 0 };
                let Some(up_bgs) = containing_background_growths(&upward_growth, &doc, base, &created_frames, &candidate, cancel)? else {
                    continue;
                };
                if !up_bgs.iter().all(|bg| {
                    enclosing_containers.iter().all(|&(c, _)| bg.target.y0 >= c.y0 + 0.5 && bg.target.y1 <= c.y1 - 0.5 && contains(c, bg.target))
                }) {
                    continue;
                }
                let mut targets = BTreeMap::from([(fit_id, shifted)]);
                for bg in &up_bgs {
                    targets.insert(bg.id, bg.target);
                }
                if !position_collision(&upward_growth, &targets, &doc, &candidate, cancel)? {
                    growth = upward_growth;
                    backgrounds = Some(up_bgs);
                    break;
                }
            }
            if backgrounds.is_none()
                && !story.text.trim().contains('\n')
                && story.text.chars().count() <= 64
                && story.text.split_whitespace().count() <= 10
                && old.height() <= 36.0
            {
                let orig_w = if prefs.dimensions_include_stroke { item.visible_bounds().width() } else { item.bounds().width() };
                let orig_h = if prefs.dimensions_include_stroke { item.visible_bounds().height() } else { item.bounds().height() };
                #[allow(clippy::type_complexity)]
                let mut widened_solution: Option<(Session, f64, f64, f64, f64, Vec<(usize, f64, f64, f64, f64)>)> = None;
                'widen: for dw in [4.0_f64, 8.0, 12.0, 18.0, 24.0, 30.0, 36.0, 48.0, 64.0] {
                    for dx_factor in [-0.5_f64, 0.0, -1.0, -0.25, -0.75, 0.5] {
                        let dx = dw * dx_factor;
                        let widened = Rect::new(old.x0 + dx, old.y0, old.x1 + dx + dw, old.y1);
                        let horiz_container_ok = |r: Rect| {
                            enclosing_containers.iter().all(|&(c, _)| {
                                let left_pad = (old.x0 - c.x0).clamp(0.0, 2.0);
                                let right_pad = (c.x1 - old.x1).clamp(0.0, 2.0);
                                r.x0 >= c.x0 + left_pad && r.x1 <= c.x1 - right_pad && contains(c, r)
                            })
                        };
                        if !contains(safe, widened) || !horiz_container_ok(widened) {
                            continue;
                        }
                        let mut pill_horiz_targets: Vec<(ItemId, usize, Rect, Rect, bool)> = Vec::with_capacity(snug_pills.len());
                        let mut pills_valid = true;
                        for (&pill_id, &(pill_idx, pill_bounds)) in &snug_pills {
                            let (pill_target, needs_widen) = if pill_bounds.x0 <= widened.x0 - 0.5 && pill_bounds.x1 >= widened.x1 + 0.5 {
                                (pill_bounds, false)
                            } else {
                                (Rect::new(pill_bounds.x0 + dx, pill_bounds.y0, pill_bounds.x1 + dx + dw, pill_bounds.y1), true)
                            };
                            if !contains(safe, pill_target)
                                || !contains(pill_target, widened)
                                || !enclosing_containers
                                    .iter()
                                    .all(|&(c, _)| pill_target.x0 >= c.x0 + 0.5 && pill_target.x1 <= c.x1 - 0.5 && contains(c, pill_target))
                            {
                                pills_valid = false;
                                break;
                            }
                            pill_horiz_targets.push((pill_id, pill_idx, pill_bounds, pill_target, needs_widen));
                        }
                        if !pills_valid {
                            continue;
                        }
                        let mut wide_targets = BTreeMap::from([(fit_id, widened)]);
                        for &(pill_id, _, _, pill_target, _) in &pill_horiz_targets {
                            wide_targets.insert(pill_id, pill_target);
                        }
                        let wide_pos = TextPosition {
                            index: fit_index,
                            id: fit_id,
                            spread: spread_index,
                            page,
                            original: old,
                            target: widened,
                            safe,
                            language: 0,
                        };
                        if position_collision(&wide_pos, &wide_targets, &doc, &candidate, cancel)? {
                            continue;
                        }
                        let mut pill_collides = false;
                        for &(pill_id, pill_idx, pill_orig, pill_target, needs_widen) in &pill_horiz_targets {
                            if !needs_widen {
                                continue;
                            }
                            let pill_pos = TextPosition {
                                index: pill_idx,
                                id: pill_id,
                                spread: spread_index,
                                page,
                                original: pill_orig,
                                target: pill_target,
                                safe,
                                language: 0,
                            };
                            if position_collision(&pill_pos, &wide_targets, &doc, &candidate, cancel)? {
                                pill_collides = true;
                                break;
                            }
                        }
                        if pill_collides {
                            continue;
                        }
                        let mut wide_session = Session::new();
                        wide_session.prefs = prefs.clone();
                        wide_session.add_document(pre_fit_state.clone());
                        let mut pill_ops = Vec::new();
                        for &(pill_id, pill_idx, _, _, needs_widen) in &pill_horiz_targets {
                            if !needs_widen {
                                continue;
                            }
                            let Some(pill_item) = pre_fit_state.doc.item(pill_id) else { continue };
                            let pill_w =
                                if prefs.dimensions_include_stroke { pill_item.visible_bounds().width() } else { pill_item.bounds().width() };
                            wide_session
                                .execute("transform.set", &json!({"ids":[pill_id.0],"width":pill_w + dw,"ref":0}))
                                .map_err(|e| e.to_string())?;
                            if dx.abs() > 0.001 {
                                wide_session.execute("transform.move", &json!({"ids":[pill_id.0],"dx":dx,"dy":0.0})).map_err(|e| e.to_string())?;
                            }
                            pill_ops.push((pill_idx, pill_w + dw, 0.0_f64, dx, 0.0_f64));
                        }
                        wide_session.execute("transform.set", &json!({"ids":[fit_id.0],"width":orig_w + dw,"ref":0})).map_err(|e| e.to_string())?;
                        if dx.abs() > 0.001 {
                            wide_session.execute("transform.move", &json!({"ids":[fit_id.0],"dx":dx,"dy":0.0})).map_err(|e| e.to_string())?;
                        }
                        let wide_doc = wide_session.doc().map_err(|e| e.to_string())?.doc.clone();
                        if !wide_session.cache.get(&wide_doc, story_id, None).is_overset() {
                            widened_solution = Some((wide_session, dw, dx, 0.0, 0.0, pill_ops));
                            break 'widen;
                        }
                        // If horizontal widening unwrapped the label to a single line whose glyph box
                        // still needs a small vertical adjustment (<= 10 pt), verify combined recovery.
                        let mut measure_wide = Session::new();
                        measure_wide.prefs = prefs.clone();
                        let wide_state = wide_session.doc().map_err(|e| e.to_string())?.clone();
                        measure_wide.add_document(wide_state.clone());
                        if measure_wide.execute("object.fit", &json!({"ids":[fit_id.0],"mode":"fitFrameToContent","axis":"height"})).is_ok()
                            && let Ok(ms_doc) = measure_wide.doc()
                            && let Some(ms_item) = ms_doc.doc.item(fit_id)
                        {
                            let rem_dh = (ms_item.bounds().height() - widened.height()).max(0.0);
                            if rem_dh > 0.0 && rem_dh <= 10.0 && !measure_wide.cache.get(&ms_doc.doc, story_id, None).is_overset() {
                                for dy_w in [0.0_f64, -rem_dh, -0.5 * rem_dh] {
                                    let comb_rect = Rect::new(widened.x0, widened.y0 + dy_w, widened.x1, widened.y1 + rem_dh + dy_w);
                                    if !contains(safe, comb_rect) || !container_ok(comb_rect) || !horiz_container_ok(comb_rect) {
                                        continue;
                                    }
                                    let mut comb_targets = BTreeMap::from([(fit_id, comb_rect)]);
                                    let mut comb_pills = Vec::with_capacity(pill_horiz_targets.len());
                                    let mut comb_pills_ok = true;
                                    for &(pill_id, pill_idx, pill_orig, pill_h_rect, needs_widen) in &pill_horiz_targets {
                                        let pill_full = if contains(pill_h_rect, comb_rect) {
                                            pill_h_rect
                                        } else {
                                            Rect::new(
                                                pill_h_rect.x0,
                                                pill_h_rect.y0 + dy_w.min(0.0),
                                                pill_h_rect.x1,
                                                pill_h_rect.y1 + (rem_dh + dy_w).max(0.0),
                                            )
                                        };
                                        if !contains(safe, pill_full)
                                            || !contains(pill_full, comb_rect)
                                            || !enclosing_containers.iter().all(|&(c, _)| {
                                                pill_full.x0 >= c.x0 + 0.5
                                                    && pill_full.x1 <= c.x1 - 0.5
                                                    && pill_full.y0 >= c.y0 + 0.5
                                                    && pill_full.y1 <= c.y1 - 0.5
                                                    && contains(c, pill_full)
                                            })
                                        {
                                            comb_pills_ok = false;
                                            break;
                                        }
                                        comb_targets.insert(pill_id, pill_full);
                                        comb_pills.push((pill_id, pill_idx, pill_orig, pill_full, needs_widen));
                                    }
                                    if !comb_pills_ok {
                                        continue;
                                    }
                                    let comb_pos = TextPosition {
                                        index: fit_index,
                                        id: fit_id,
                                        spread: spread_index,
                                        page,
                                        original: old,
                                        target: comb_rect,
                                        safe,
                                        language: 0,
                                    };
                                    if position_collision(&comb_pos, &comb_targets, &doc, &candidate, cancel)? {
                                        continue;
                                    }
                                    let mut comb_pill_collision = false;
                                    for &(pill_id, pill_idx, pill_orig, pill_full, _) in &comb_pills {
                                        if pill_full == pill_orig {
                                            continue;
                                        }
                                        let pp = TextPosition {
                                            index: pill_idx,
                                            id: pill_id,
                                            spread: spread_index,
                                            page,
                                            original: pill_orig,
                                            target: pill_full,
                                            safe,
                                            language: 0,
                                        };
                                        if position_collision(&pp, &comb_targets, &doc, &candidate, cancel)? {
                                            comb_pill_collision = true;
                                            break;
                                        }
                                    }
                                    if comb_pill_collision {
                                        continue;
                                    }
                                    let mut comb_session = Session::new();
                                    comb_session.prefs = prefs.clone();
                                    comb_session.add_document(pre_fit_state.clone());
                                    let mut comb_pill_ops = Vec::new();
                                    for &(pill_id, pill_idx, pill_orig, pill_full, needs_widen) in &comb_pills {
                                        let Some(pill_item) = pre_fit_state.doc.item(pill_id) else { continue };
                                        let base_pw = if prefs.dimensions_include_stroke {
                                            pill_item.visible_bounds().width()
                                        } else {
                                            pill_item.bounds().width()
                                        };
                                        let base_ph = if prefs.dimensions_include_stroke {
                                            pill_item.visible_bounds().height()
                                        } else {
                                            pill_item.bounds().height()
                                        };
                                        let p_dw = pill_full.width() - pill_orig.width();
                                        let p_dh = pill_full.height() - pill_orig.height();
                                        let p_dx = pill_full.x0 - pill_orig.x0;
                                        let p_dy = pill_full.y0 - pill_orig.y0;
                                        if needs_widen || p_dw.abs() > 0.001 || p_dh.abs() > 0.001 {
                                            let mut set_obj = serde_json::Map::new();
                                            set_obj.insert("ids".into(), json!([pill_id.0]));
                                            set_obj.insert("ref".into(), json!(0));
                                            if needs_widen || p_dw.abs() > 0.001 {
                                                set_obj.insert("width".into(), json!(base_pw + p_dw));
                                            }
                                            if p_dh.abs() > 0.001 {
                                                set_obj.insert("height".into(), json!(base_ph + p_dh));
                                            }
                                            comb_session.execute("transform.set", &Value::Object(set_obj)).map_err(|e| e.to_string())?;
                                        }
                                        if p_dx.abs() > 0.001 || p_dy.abs() > 0.001 {
                                            comb_session
                                                .execute("transform.move", &json!({"ids":[pill_id.0],"dx":p_dx,"dy":p_dy}))
                                                .map_err(|e| e.to_string())?;
                                        }
                                        if needs_widen || p_dw.abs() > 0.001 || p_dh.abs() > 0.001 || p_dx.abs() > 0.001 || p_dy.abs() > 0.001 {
                                            comb_pill_ops.push((
                                                pill_idx,
                                                if needs_widen || p_dw.abs() > 0.001 { base_pw + p_dw } else { 0.0 },
                                                if p_dh.abs() > 0.001 { base_ph + p_dh } else { 0.0 },
                                                p_dx,
                                                p_dy,
                                            ));
                                        }
                                    }
                                    comb_session
                                        .execute(
                                            "transform.set",
                                            &json!({"ids":[fit_id.0],"width":orig_w + dw,"height":orig_h + rem_dh + 0.01,"ref":0}),
                                        )
                                        .map_err(|e| e.to_string())?;
                                    if dx.abs() > 0.001 || dy_w.abs() > 0.001 {
                                        comb_session
                                            .execute("transform.move", &json!({"ids":[fit_id.0],"dx":dx,"dy":dy_w}))
                                            .map_err(|e| e.to_string())?;
                                    }
                                    let comb_doc = comb_session.doc().map_err(|e| e.to_string())?.doc.clone();
                                    if !comb_session.cache.get(&comb_doc, story_id, None).is_overset() {
                                        widened_solution = Some((comb_session, dw, dx, rem_dh + 0.01, dy_w, comb_pill_ops));
                                        break 'widen;
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some((wide_session, dw, dx, dh_extra, dy_extra, pill_ops)) = widened_solution {
                    let pill_cmd_count: usize = pill_ops
                        .iter()
                        .map(|&(_, w, h, p_dx, p_dy)| usize::from(w > 0.0 || h > 0.0) + usize::from(p_dx.abs() > 0.001 || p_dy.abs() > 0.001))
                        .sum();
                    let needed_cmds = pill_cmd_count.saturating_add(1).saturating_add(usize::from(dx.abs() > 0.001 || dy_extra.abs() > 0.001));
                    if candidate.commands.len().checked_add(needed_cmds).is_none_or(|len| len > 512) {
                        return Err("Local fitting exceeds the bounded native action limit".into());
                    }
                    scratch = wide_session;
                    for (pill_idx, new_w, new_h, p_dx, p_dy) in pill_ops {
                        let pill_ref = format!("${pill_idx}.id");
                        if new_w > 0.0 || new_h > 0.0 {
                            let mut set_obj = serde_json::Map::new();
                            set_obj.insert("ids".into(), json!([pill_ref.clone()]));
                            set_obj.insert("ref".into(), json!(0));
                            if new_w > 0.0 {
                                set_obj.insert("width".into(), json!(new_w));
                            }
                            if new_h > 0.0 {
                                set_obj.insert("height".into(), json!(new_h));
                            }
                            candidate.commands.push(EngineAction { command: "transform.set".into(), params: Value::Object(set_obj) });
                        }
                        if p_dx.abs() > 0.001 || p_dy.abs() > 0.001 {
                            candidate
                                .commands
                                .push(EngineAction { command: "transform.move".into(), params: json!({"ids":[pill_ref],"dx":p_dx,"dy":p_dy}) });
                        }
                    }
                    let mut fit_set = serde_json::Map::new();
                    fit_set.insert("ids".into(), json!([fit_item_ref.clone()]));
                    fit_set.insert("width".into(), json!(orig_w + dw));
                    if dh_extra > 0.0 {
                        fit_set.insert("height".into(), json!(orig_h + dh_extra));
                    }
                    fit_set.insert("ref".into(), json!(0));
                    candidate.commands.push(EngineAction { command: "transform.set".into(), params: Value::Object(fit_set) });
                    if dx.abs() > 0.001 || dy_extra.abs() > 0.001 {
                        candidate
                            .commands
                            .push(EngineAction { command: "transform.move".into(), params: json!({"ids":[fit_item_ref],"dx":dx,"dy":dy_extra}) });
                    }
                    notes.push(json!({
                        "kind": "nativeTextReflow",
                        "page": spread_index,
                        "item": fit_id.0,
                        "label": item.label,
                        "extraWidthPt": dw,
                        "extraHeightPt": dh_extra,
                        "dx": dx,
                        "dy": dy_extra,
                        "message": "Widened wrapped short label into verified safe horizontal space; preserved copy and font size"
                    }));
                    continue;
                }
            }
        }
        let Some(backgrounds) = backgrounds.filter(|_| !scratch.cache.get(&fitted_doc, story_id, None).is_overset()) else {
            return Err(format!(
                "{identity} has overset text and needs {:.1} pt more height, but no safe free space remains; copy and font size were preserved. Reflow this activity before buying images",
                (measured.height() - old.height()).max(0.0)
            ));
        };
        let dy = growth.target.y0 - grown.y0;
        let has_later_geometry_op = candidate.commands.get(fit_index.saturating_add(1)..).is_some_and(|tail| {
            tail.iter().any(|cmd| {
                matches!(
                    cmd.command.as_str(),
                    "transform.set"
                        | "transform.move"
                        | "transform.scale"
                        | "transform.rotate"
                        | "transform.shear"
                        | "transform.flip"
                        | "transform.clear"
                        | "object.fit"
                        | "object.align"
                        | "object.distribute"
                        | "object.group"
                        | "style.object.apply"
                )
            })
        });
        let needs_explicit_height_set = fit_is_existing_base || has_later_geometry_op || loc.path.len() > 1;
        let bg_cmd_count: usize = backgrounds
            .iter()
            .map(|bg| {
                usize::from((bg.target.height() - bg.original.height()).abs() > 0.001) + usize::from((bg.target.y0 - bg.original.y0).abs() > 0.001)
            })
            .sum();
        if candidate
            .commands
            .len()
            .checked_add(bg_cmd_count)
            .and_then(|length| length.checked_add(usize::from(needs_explicit_height_set)))
            .and_then(|length| length.checked_add(usize::from(dy != 0.0)))
            .is_none_or(|length| length > 512)
        {
            return Err("Local fitting exceeds the bounded native action limit".into());
        }
        for background in backgrounds {
            let current = scratch.doc().map_err(|error| error.to_string())?.doc.item(background.id).ok_or("Native background disappeared")?;
            let bg_extra_h = background.target.height() - background.original.height();
            let bg_dy = background.target.y0 - background.original.y0;
            let bg_ref = format!("${}.id", background.index);
            if bg_extra_h.abs() > 0.001 {
                let height =
                    (if prefs.dimensions_include_stroke { current.visible_bounds().height() } else { current.bounds().height() }) + bg_extra_h;
                let params = json!({"ids":[background.id.0],"height":height,"ref":0});
                scratch.execute("transform.set", &params).map_err(|error| error.to_string())?;
                candidate
                    .commands
                    .push(EngineAction { command: "transform.set".into(), params: json!({"ids":[bg_ref.clone()],"height":height,"ref":0}) });
            }
            if bg_dy.abs() > 0.001 {
                scratch.execute("transform.move", &json!({"ids":[background.id.0],"dx":0.0,"dy":bg_dy})).map_err(|error| error.to_string())?;
                candidate.commands.push(EngineAction { command: "transform.move".into(), params: json!({"ids":[bg_ref],"dx":0.0,"dy":bg_dy}) });
            }
            notes.push(json!({"kind":"nativeBackgroundReflow","page":spread_index,"item":background.id.0,"extraHeightPt":bg_extra_h,"dy":bg_dy,
                "message":"Expanded a new rectangular text container into verified free space; preserved its padding and existing artwork"}));
        }
        if needs_explicit_height_set {
            let fitted_height =
                (if prefs.dimensions_include_stroke { fitted.visible_bounds().height() } else { fitted.bounds().height() }) + mark_pad;
            scratch.execute("transform.set", &json!({"ids":[fit_id.0],"height":fitted_height,"ref":0})).map_err(|error| error.to_string())?;
            candidate
                .commands
                .push(EngineAction { command: "transform.set".into(), params: json!({"ids":[fit_item_ref.clone()],"height":fitted_height,"ref":0}) });
        }
        if dy != 0.0 {
            scratch.execute("transform.move", &json!({"ids":[fit_id.0],"dx":0,"dy":dy})).map_err(|error| error.to_string())?;
            candidate.commands.push(EngineAction { command: "transform.move".into(), params: json!({"ids":[fit_item_ref.clone()],"dx":0,"dy":dy}) });
            notes.push(json!({"kind":"nativeTextTranslation","page":spread_index,"item":fit_id.0,"dx":0,"dy":dy,
                "message":"Moved newly fitted text upward into verified safe space; preserved copy, font sizes and explicit text insets"}));
        }
        if !needs_explicit_height_set {
            let rect = candidate
                .commands
                .get_mut(fit_index)
                .and_then(|a| a.params.get_mut("rect"))
                .and_then(Value::as_array_mut)
                .ok_or("Native text frame has no source rectangle")?;
            let bottom = rect.get(3).and_then(Value::as_f64).ok_or("Invalid native frame height")?;
            let value = rect.get_mut(3).ok_or("Invalid native frame rectangle")?;
            *value = json!(bottom + extra_height);
        }
        notes.push(json!({"kind":"nativeTextReflow","page":spread_index,"item":fit_id.0,"label":item.label,"extraHeightPt":extra_height,"message":"Expanded text frame into verified free space; preserved all text and font size"}));
    }
    let existing_story_ids: Vec<StoryId> = scratch
        .doc()
        .map_err(|e| e.to_string())?
        .doc
        .stories
        .keys()
        .copied()
        .filter(|sid| !seen.contains(sid) && !pre_existing_overset.contains(sid))
        .collect();
    for story_id in existing_story_ids {
        if cancel.load(Ordering::Relaxed) {
            return Err("Local layout cancelled".into());
        }
        let doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
        if let Some(story) = doc.story(story_id) {
            let modified = base
                .doc
                .story(story_id)
                .is_none_or(|before| before.text != story.text || serde_json::to_value(before).ok() != serde_json::to_value(story).ok());
            if modified && let Some(&frame_id) = story.frames.last() {
                apply_story_glyph_fallbacks(story_id, frame_id, &json!(story_id.0), &mut scratch, &mut candidate, &mut notes)?;
            }
        }
        let doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
        if !scratch.cache.get(&doc, story_id, None).is_overset() {
            continue;
        }
        let story = doc.story(story_id).ok_or("Native layout story disappeared")?;
        let Some(&fit_id) = story.frames.last() else {
            continue;
        };
        let loc = doc.find(fit_id).ok_or("Native text frame location disappeared")?;
        let designcraft_engine::doc::SpreadRef::Doc(spread_index) = loc.spread else {
            return Err("Cannot fit a parent-page frame in production".into());
        };
        let spread = doc.spreads.get(spread_index).ok_or("Native page disappeared")?;
        let item = doc.item(fit_id).ok_or("Native frame disappeared")?;
        let outer = doc.parent_xf(&loc);
        let old = outer.transform_rect_bbox(item.bounds());
        let identity = frame_identity(spread_index, 0, fit_id, old, &story.text);
        let linear = (outer * item.xf).as_coeffs();
        if (linear[0] - 1.0).abs() > 0.001 || linear[1].abs() > 0.001 || linear[2].abs() > 0.001 || (linear[3] - 1.0).abs() > 0.001 {
            return Err(format!("{identity} has overset text in a transformed frame; reflow its layout before generation"));
        }
        scratch.execute("object.fit", &json!({"ids":[fit_id.0],"mode":"fitFrameToContent","axis":"height"})).map_err(|e| e.to_string())?;
        let fitted_doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
        let fitted = fitted_doc.item(fit_id).ok_or("Fitted text frame disappeared")?;
        let measured = outer.transform_rect_bbox(fitted.bounds());
        let extra_height = measured.height() - old.height() + 0.01;
        let grown = Rect::new(measured.x0, measured.y0, measured.x1, measured.y1 + 0.01);
        let page = spread.page_at_x(old.center().x).ok_or("Native text frame has no owning page")?;
        let safe = spread.pages.get(page).ok_or("Native text page disappeared")?.margin_rect();
        let growth = TextPosition { index: 0, id: fit_id, spread: spread_index, page, original: old, target: grown, safe, language: 0 };
        let blocked = !extra_height.is_finite()
            || extra_height <= 0.0
            || !is_allowed_fit_location(&doc, &loc)
            || item.locked
            || item.hidden
            || item.nonprinting
            || doc.layer(item.layer).is_some_and(|layer| layer.locked || !layer.visible || !layer.printable)
            || !contains(safe, grown);
        let backgrounds = if blocked { None } else { containing_background_growths(&growth, &doc, base, &created_frames, &candidate, cancel)? };
        let Some(backgrounds) = backgrounds.filter(|_| !scratch.cache.get(&fitted_doc, story_id, None).is_overset()) else {
            return Err(format!(
                "{identity} has overset text and needs {:.1} pt more height, but no safe free space remains; copy and font size were preserved. Reflow this activity before buying images",
                (measured.height() - old.height()).max(0.0)
            ));
        };
        if candidate.commands.len().checked_add(backgrounds.len()).and_then(|len| len.checked_add(1)).is_none_or(|len| len > 512) {
            return Err("Local fitting exceeds the bounded native action limit".into());
        }
        for background in backgrounds {
            let current = scratch.doc().map_err(|error| error.to_string())?.doc.item(background.id).ok_or("Native background disappeared")?;
            let height = if prefs.dimensions_include_stroke { current.visible_bounds().height() } else { current.bounds().height() } + extra_height;
            let params = json!({"ids":[background.id.0],"height":height,"ref":0});
            scratch.execute("transform.set", &params).map_err(|error| error.to_string())?;
            candidate.commands.push(EngineAction {
                command: "transform.set".into(),
                params: json!({"ids":[format!("${}.id",background.index)],"height":height,"ref":0}),
            });
            notes.push(json!({"kind":"nativeBackgroundReflow","page":spread_index,"item":background.id.0,"extraHeightPt":extra_height,
                "message":"Expanded a new rectangular text container into verified free space; preserved its padding and existing artwork"}));
        }
        let fitted_height = (if prefs.dimensions_include_stroke { fitted.visible_bounds().height() } else { fitted.bounds().height() }) + 0.01;
        scratch.execute("transform.set", &json!({"ids":[fit_id.0],"height":fitted_height,"ref":0})).map_err(|error| error.to_string())?;
        candidate.commands.push(EngineAction { command: "transform.set".into(), params: json!({"ids":[fit_id.0],"height":fitted_height,"ref":0}) });
        notes.push(json!({"kind":"nativeTextReflow","page":spread_index,"item":fit_id.0,"label":item.label,"extraHeightPt":extra_height,"message":"Expanded existing text frame into verified free space; preserved all text and font size"}));
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
    check_repeated_slots(&candidate, verified.doc().map_err(|e| e.to_string())?, cancel)?;
    *plan = candidate;
    Ok(notes)
}

pub(crate) fn is_fallback_symbol_or_honorific(ch: char) -> bool {
    if ch.is_whitespace() || ch.is_control() {
        return false;
    }
    matches!(
        ch,
        '©'
            | '®'
            | '°'
            | '±'
            | '²'
            | '³'
            | '¹'
            | '¼'..='¾'
            | '×'
            | '÷'
            | '\u{0600}'..='\u{0605}'
            | '\u{060e}'..='\u{061a}'
            | '\u{06d6}'..='\u{06ed}'
            | '\u{06fd}'..='\u{06fe}'
            | '\u{08d3}'..='\u{08ff}'
            | '\u{2000}'..='\u{2bff}'
            | '\u{2e00}'..='\u{2e7f}'
            | '\u{3000}'..='\u{303f}'
            | '\u{fd3e}'..='\u{fd4f}'
            | '\u{fdcf}'
            | '\u{fdf0}'..='\u{fdff}'
            | '\u{fe30}'..='\u{fe6b}'
            | '\u{fff0}'..='\u{fffd}'
            | '\u{1f000}'..='\u{1faff}'
    )
}

fn is_variation_selector(ch: char) -> bool {
    matches!(ch, '\u{fe00}'..='\u{fe0f}')
}

pub(crate) fn has_fallback_glyph(db: &designcraft_fonts::ScopedFonts<'_>, ch: char) -> bool {
    resolve_fallback_face(db, ch, false, false).is_some()
}

fn resolve_fallback_face(db: &designcraft_fonts::ScopedFonts<'_>, ch: char, bold: bool, italic: bool) -> Option<(String, String)> {
    const PREFERRED: &[&str] = &[
        "Segoe UI Symbol",
        "Segoe UI Historic",
        "Segoe UI Emoji",
        "Traditional Arabic",
        "Sakkal Majalla",
        "Simplified Arabic",
        "Arial",
        "Noto Sans Symbols 2",
        "Noto Sans Symbols",
        "Noto Emoji",
        "Amiri",
        "Noto Naskh Arabic",
        "DejaVu Sans",
        "Apple Symbols",
    ];
    let check_family = |family: &str| -> Option<(String, String)> {
        let styles = db.styles(family);
        if styles.is_empty() {
            return None;
        }
        if let Some(style) = styles.iter().find(|s| {
            let (weight, slanted) = db.traits_of(family, s);
            (weight >= 600.0) == bold && slanted == italic && db.face(family, s).covers(ch)
        }) {
            return Some((family.to_owned(), style.clone()));
        }
        if let Some(style) =
            styles.iter().find(|s| matches!(s.as_str(), "Regular" | "Roman" | "Book" | "Normal" | "Medium") && db.face(family, s).covers(ch))
        {
            return Some((family.to_owned(), style.clone()));
        }
        styles.iter().find(|s| db.face(family, s).covers(ch)).map(|s| (family.to_owned(), s.clone()))
    };
    for &family in PREFERRED {
        if let Some(found) = check_family(family) {
            return Some(found);
        }
    }
    for family in db.families() {
        if let Some(found) = check_family(&family) {
            return Some(found);
        }
    }
    None
}

fn is_combining_arabic_mark(ch: char) -> bool {
    matches!(ch, '\u{0610}'..='\u{061a}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06ed}' | '\u{08d3}'..='\u{08ff}')
}

fn resolve_cluster_fallback_face(db: &designcraft_fonts::ScopedFonts<'_>, cluster: &str, bold: bool, italic: bool) -> Option<(String, String)> {
    const ARABIC_PREFERRED: &[&str] = &[
        "Traditional Arabic",
        "Sakkal Majalla",
        "Simplified Arabic",
        "Amiri",
        "Noto Naskh Arabic",
        "Segoe UI Historic",
        "Arial",
        "Segoe UI Symbol",
        "DejaVu Sans",
    ];
    let covers_all = |family: &str, style: &str| {
        let face = db.face(family, style);
        cluster.chars().filter(|&c| !is_variation_selector(c)).all(|c| face.covers(c))
    };
    let check_family = |family: &str| -> Option<(String, String)> {
        let styles = db.styles(family);
        if styles.is_empty() {
            return None;
        }
        if let Some(style) = styles.iter().find(|s| {
            let (weight, slanted) = db.traits_of(family, s);
            (weight >= 600.0) == bold && slanted == italic && covers_all(family, s)
        }) {
            return Some((family.to_owned(), style.clone()));
        }
        if let Some(style) = styles.iter().find(|s| matches!(s.as_str(), "Regular" | "Roman" | "Book" | "Normal" | "Medium") && covers_all(family, s))
        {
            return Some((family.to_owned(), style.clone()));
        }
        styles.iter().find(|s| covers_all(family, s)).map(|s| (family.to_owned(), s.clone()))
    };
    for &family in ARABIC_PREFERRED {
        if let Some(found) = check_family(family) {
            return Some(found);
        }
    }
    for family in db.families() {
        if let Some(found) = check_family(&family) {
            return Some(found);
        }
    }
    None
}

fn apply_story_glyph_fallbacks(
    story_id: StoryId,
    item_id: ItemId,
    story_ref: &Value,
    scratch: &mut Session,
    candidate: &mut ProductionPlan,
    notes: &mut Vec<Value>,
) -> Result<(), String> {
    let doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
    let story = doc.story(story_id).ok_or("Native layout story disappeared")?;
    let db = designcraft_fonts::FontDb::global().scoped(doc.font_scope);
    let mut fallbacks: Vec<(usize, usize, String, String, String)> = Vec::new();
    for (pi, range) in story.para_ranges().into_iter().enumerate() {
        let para = story.paras.get(pi).ok_or("Native paragraph formatting disappeared")?;
        let (_, base_chars) = doc.styles.resolve_para(para);
        let text = story.text.get(range.clone()).ok_or("Invalid native paragraph boundary")?;
        let mut iter = text.char_indices().peekable();
        let mut last_base: Option<(usize, char)> = None;
        while let Some((offset, ch)) = iter.next() {
            let byte = range.start.saturating_add(offset);
            let mut end = byte.saturating_add(ch.len_utf8());
            let combining_mark = is_combining_arabic_mark(ch);
            if !combining_mark && !is_variation_selector(ch) {
                last_base = Some((byte, ch));
            }
            // Only decorative symbols, Arabic honorific ligatures/signs, and missing Arabic
            // combining marks get a fallback. Ordinary copy and Arabic letterforms retain the
            // selected family; a missing requested font remains an explicit error.
            if !is_fallback_symbol_or_honorific(ch) && !combining_mark {
                if is_variation_selector(ch) {
                    let attrs = doc.styles.resolve_char(&base_chars, story.char_format_of(byte));
                    if !db.face(&attrs.font_family, &attrs.font_style).covers(ch) {
                        let (weight, italic) = db.traits_of(&attrs.font_family, &attrs.font_style);
                        if let Some((family, style)) = resolve_fallback_face(&db, ch, weight >= 600.0, italic) {
                            fallbacks.push((byte, end, ch.to_string(), family, style));
                        }
                    }
                }
                continue;
            }
            let attrs = doc.styles.resolve_char(&base_chars, story.char_format_of(byte));
            if db.face(&attrs.font_family, &attrs.font_style).covers(ch) {
                continue;
            }
            let (weight, italic) = db.traits_of(&attrs.font_family, &attrs.font_style);
            let mut span_start = byte;
            let mut cluster_chars = String::new();
            if combining_mark && let Some((base_byte, base_ch)) = last_base {
                span_start = base_byte;
                cluster_chars.push(base_ch);
            }
            cluster_chars.push(ch);
            while let Some(&(next_offset, next_ch)) = iter.peek() {
                if !(is_variation_selector(next_ch) || (combining_mark && is_combining_arabic_mark(next_ch))) {
                    break;
                }
                end = range.start.saturating_add(next_offset).saturating_add(next_ch.len_utf8());
                cluster_chars.push(next_ch);
                let _ = iter.next();
            }
            let resolved = if combining_mark && span_start < byte {
                resolve_cluster_fallback_face(&db, &cluster_chars, weight >= 600.0, italic).or_else(|| {
                    resolve_fallback_face(&db, ch, weight >= 600.0, italic).map(|(f, s)| {
                        span_start = byte;
                        (f, s)
                    })
                })
            } else {
                resolve_fallback_face(&db, ch, weight >= 600.0, italic)
            };
            let (family, style) = resolved.ok_or_else(|| format!("Native text frame {} needs an installed font covering symbol {ch}", item_id.0))?;
            if let Some((_, prev_end, prev_chars, prev_family, prev_style)) = fallbacks.last_mut()
                && *prev_end >= span_start
                && *prev_family == family
                && *prev_style == style
            {
                *prev_end = end.max(*prev_end);
                prev_chars.push_str(&cluster_chars);
            } else {
                fallbacks.push((span_start, end, cluster_chars, family, style));
                if fallbacks.len() > 256 {
                    return Err("Too many symbol fallback ranges in one native story".into());
                }
            }
        }
    }
    for (start, end, symbols, family, style) in fallbacks {
        if candidate.commands.len().checked_add(2).is_none_or(|len| len > 512) {
            return Err("Symbol glyph fallback exceeds the bounded native action limit".into());
        }
        let actions = [
            EngineAction { command: "text.select".into(), params: json!({"story":story_ref,"anchor":start,"focus":end}) },
            EngineAction { command: "type.char".into(), params: json!({"attrs":{"fontFamily":family,"fontStyle":style}}) },
        ];
        scratch.execute("text.select", &json!({"story":story_id.0,"anchor":start,"focus":end})).map_err(|e| e.to_string())?;
        let char_action = actions.get(1).ok_or("Missing symbol fallback formatting action")?;
        scratch.execute("type.char", &char_action.params).map_err(|e| e.to_string())?;
        candidate.commands.extend(actions);
        notes.push(json!({
            "kind":"symbolFontFallback",
            "item":item_id.0,
            "symbol":symbols,
            "fontFamily":family,
            "fontStyle":style,
            "message":"Preserved the character using an installed symbol or honorific fallback font"
        }));
    }
    Ok(())
}

fn normalize_effects_aliases(effects: &mut serde_json::Map<String, Value>) {
    if !effects.contains_key("bevel")
        && let Some(mut bevel) = effects.remove("bevelEmboss")
    {
        if let Some(b_obj) = bevel.as_object_mut() {
            if !b_obj.contains_key("depth")
                && let Some(soften) = b_obj.remove("soften")
            {
                b_obj.insert("depth".into(), soften);
            } else {
                b_obj.remove("soften");
            }
            b_obj.remove("altitude");
        }
        effects.insert("bevel".into(), bevel);
    }
    if let Some(df) = effects.get_mut("directionalFeather")
        && df.is_array()
    {
        let widths = df.take();
        *df = json!({"on": true, "widths": widths});
    }
    for key in ["innerShadow", "innerGlow"] {
        if let Some(sub) = effects.get_mut(key).and_then(Value::as_object_mut)
            && !sub.contains_key("choke")
            && let Some(spread) = sub.remove("spread")
        {
            sub.insert("choke".into(), spread);
        }
    }
}

fn normalize_alt_fills_aliases(alt: &mut serde_json::Map<String, Value>) {
    for (alias, canonical) in
        [("everyA", "first"), ("colorA", "firstColor"), ("tintA", "firstTint"), ("everyB", "next"), ("colorB", "nextColor"), ("tintB", "nextTint")]
    {
        if let Some(val) = alt.remove(alias) {
            alt.entry(canonical).or_insert(val);
        }
    }
}

/// Normalize common model-emitted command aliases (such as `type.set` -> `story.setText`,
/// `style.para.*` -> `style.paragraph.*`, `style.char.*` -> `style.character.*`,
/// `pathfinder.*` -> `object.pathfinder`, `qr.create` -> `object.qrCode`) before validation.
pub(crate) fn normalize_command_aliases(plan: &mut ProductionPlan) -> Vec<Value> {
    let mut notes = Vec::new();
    for (index, action) in plan.commands.iter_mut().enumerate() {
        let original = action.command.trim();
        let has_style_name = action.params.get("name").is_some() || action.params.get("style").is_some();
        let (canonical, inferred_op, inferred_list_type) = match original {
            "type.set" | "text.set" | "story.set" => (Some("story.setText"), None, None),
            "object.delete" | "item.delete" | "frame.delete" | "edit.delete" => (Some("edit.clear"), None, None),
            "object.move" => (Some("transform.move"), None, None),
            "object.resize" => (Some("transform.set"), None, None),
            "object.cornerRadius" | "object.corners" => (Some("object.cornerOptions"), None, None),
            "text.replace" | "find.replace" => (Some("find.change"), None, None),
            "style.para.create" => (Some("style.paragraph.create"), None, None),
            "style.para.edit" => (Some("style.paragraph.edit"), None, None),
            "style.para.apply" => (Some("style.paragraph.apply"), None, None),
            "style.char.create" => (Some("style.character.create"), None, None),
            "style.char.edit" => (Some("style.character.edit"), None, None),
            "style.char.apply" => (Some("style.character.apply"), None, None),
            "lists.bulleted" | "list.bulleted" | "type.bulletedList" => (Some("type.para"), None, Some("bullets")),
            "lists.numbered" | "list.numbered" | "type.numberedList" => (Some("type.para"), None, Some("numbers")),
            "pathfinder.add" => (Some("object.pathfinder"), Some("add"), None),
            "pathfinder.subtract" => (Some("object.pathfinder"), Some("subtract"), None),
            "pathfinder.intersect" => (Some("object.pathfinder"), Some("intersect"), None),
            "pathfinder.exclude" => (Some("object.pathfinder"), Some("exclude"), None),
            "pathfinder.minusBack" => (Some("object.pathfinder"), Some("minusBack"), None),
            "object.compoundPath.make" | "compoundPath.make" => (Some("object.makeCompoundPath"), None, None),
            "object.compoundPath.release" | "compoundPath.release" => (Some("object.releaseCompoundPath"), None, None),
            "object.gradientFill" | "gradient.fill" => (Some("object.gradient"), None, None),
            "qr.create" | "qrcode.create" => (Some("object.qrCode"), None, None),
            "page.insert" | "pages.insert" => (Some("layout.pages.insert"), None, None),
            "table.set_cell" | "table.cell" => (Some("table.setCell"), None, None),
            "table.insertCol" => (Some("table.insertColumn"), None, None),
            "table.deleteCol" => (Some("table.deleteColumn"), None, None),
            "table.rowHeight" => (Some("table.setRowHeight"), None, None),
            "table.columnWidth" | "table.colWidth" => (Some("table.setColumnWidth"), None, None),
            "table.mergeCells" => (Some("table.merge"), None, None),
            "table.unmergeCells" => (Some("table.unmerge"), None, None),
            "table.setCellStyle" => (Some("style.cell.apply"), None, None),
            "table.setTableStyle" if has_style_name => (Some("style.table.apply"), None, None),
            "table.setTableStyle" | "table.setHeaderFooterRows" => (Some("table.options"), None, None),
            "object.stepAndRepeat" | "transform.stepAndRepeat" => (Some("edit.stepAndRepeat"), None, None),
            _ => (None, None, None),
        };
        if let Some(target) = canonical {
            let requested = action.command.clone();
            action.command = target.to_owned();
            if let Some(op) = inferred_op
                && let Some(obj) = action.params.as_object_mut()
                && !obj.contains_key("op")
            {
                obj.insert("op".into(), Value::String(op.into()));
            }
            if let Some(lt) = inferred_list_type
                && let Some(obj) = action.params.as_object_mut()
            {
                let attrs = obj.entry("attrs").or_insert_with(|| json!({}));
                if let Some(attrs_obj) = attrs.as_object_mut() {
                    attrs_obj.entry("listType").or_insert_with(|| Value::String(lt.into()));
                }
            }
            notes.push(json!({
                "kind": "commandAliasNormalized",
                "command": index,
                "requested": requested,
                "resolved": target,
                "message": "Normalized command alias to canonical vetted engine command"
            }));
        }
        if action.command == "frame.create"
            && let Some(obj) = action.params.as_object_mut()
            && !obj.contains_key("content")
            && obj.get("text").and_then(Value::as_str).is_some()
        {
            obj.insert("content".into(), Value::String("text".into()));
            notes.push(json!({
                "kind": "commandAliasNormalized",
                "command": index,
                "requested": "frame.create",
                "resolved": "frame.create(content=text)",
                "message": "Inferred text content type for frame.create carrying inline text"
            }));
        }
        if action.command == "object.qrCode"
            && let Some(obj) = action.params.as_object_mut()
            && !obj.contains_key("content")
            && let Some(text) = obj.remove("text")
        {
            obj.insert("content".into(), text);
        }
        if action.command == "object.cornerOptions"
            && let Some(obj) = action.params.as_object_mut()
        {
            if !obj.contains_key("size")
                && let Some(radius) = obj.remove("radius")
            {
                obj.insert("size".into(), radius);
            } else {
                obj.remove("radius");
            }
            obj.entry("shape").or_insert_with(|| Value::String("rounded".into()));
        }
        if matches!(
            action.command.as_str(),
            "style.cell.apply" | "style.table.apply" | "style.object.apply" | "style.paragraph.apply" | "style.character.apply"
        ) && let Some(obj) = action.params.as_object_mut()
            && !obj.contains_key("name")
            && let Some(style) = obj.remove("style")
        {
            obj.insert("name".into(), style);
        }
        if action.command == "story.setText"
            && let Some(obj) = action.params.as_object_mut()
        {
            if !obj.contains_key("story")
                && let Some(id) = obj.remove("id")
            {
                obj.insert("story".into(), id);
            }
            if !obj.contains_key("text")
                && let Some(content) = obj.remove("content")
            {
                obj.insert("text".into(), content);
            }
        }
        if action.command == "object.gradient"
            && let Some(stops) = action.params.get_mut("stops").and_then(Value::as_array_mut)
        {
            for stop in stops {
                if let Some(st) = stop.as_object_mut()
                    && let Some(pos) = st.remove("pos")
                    && !st.contains_key("location")
                    && let Some(v) = pos.as_f64()
                {
                    let loc = if v <= 1.0 { v * 100.0 } else { v };
                    st.insert("location".into(), json!(loc));
                }
            }
        }
        if matches!(action.command.as_str(), "object.effects" | "style.object.create")
            && let Some(effects) = action.params.get_mut("effects").and_then(Value::as_object_mut)
        {
            normalize_effects_aliases(effects);
        }
        if matches!(action.command.as_str(), "style.table.create" | "table.options")
            && let Some(obj) = action.params.as_object_mut()
        {
            for repeat_key in ["repeatHeader", "repeatFooter"] {
                if let Some(val) = obj.get_mut(repeat_key)
                    && let Some(s) = val.as_str()
                {
                    *val = Value::Bool(!matches!(s, "none" | "false" | "off"));
                }
            }
            for alt_key in ["altRows", "altCols"] {
                if let Some(alt) = obj.get_mut(alt_key).and_then(Value::as_object_mut) {
                    normalize_alt_fills_aliases(alt);
                }
            }
        }
        if action.command == "object.align"
            && let Some(obj) = action.params.as_object_mut()
        {
            if !obj.contains_key("edge")
                && let Some(align_val) = obj.remove("align")
            {
                obj.insert("edge".into(), align_val);
            }
            if let Some(edge_slot) = obj.get_mut("edge") {
                match edge_slot.as_str() {
                    Some("center") => *edge_slot = Value::String("hcenter".into()),
                    Some("middle") => *edge_slot = Value::String("vcenter".into()),
                    _ => {}
                }
            }
        }
        for container_key in ["para", "attrs"] {
            if let Some(para) = action.params.get_mut(container_key).and_then(Value::as_object_mut)
                && let Some(lt) = para.get_mut("listType")
            {
                let normalized = match lt.as_str() {
                    Some("bullet") => Some("bullets"),
                    Some("numbered" | "number") => Some("numbers"),
                    _ => None,
                };
                if let Some(target_lt) = normalized {
                    *lt = Value::String(target_lt.into());
                }
            }
        }
    }
    fn parse_step_field(raw: &str, current_idx: usize) -> Option<(usize, &str)> {
        let body = raw.strip_prefix("${").and_then(|b| b.strip_suffix('}')).or_else(|| raw.strip_prefix('$'))?;
        let (step_str, field) = body.split_once('.')?;
        let resolved_step = if step_str == "last" { current_idx.checked_sub(1)? } else { step_str.parse::<usize>().ok()? };
        (resolved_step < current_idx).then_some((resolved_step, field))
    }
    let is_text_frame_action = |a: &EngineAction| -> bool {
        a.command == "frame.create"
            && (a.params.get("content").and_then(Value::as_str) == Some("text") || a.params.get("text").and_then(Value::as_str).is_some())
    };
    let is_non_text_item_creation = |a: &EngineAction| -> bool {
        (a.command == "frame.create" && !is_text_frame_action(a))
            || matches!(a.command.as_str(), "shape.create" | "line.create" | "path.create" | "object.qrCode")
    };
    let mut placed_frame_steps: BTreeSet<usize> = BTreeSet::new();
    for (idx, a) in plan.commands.iter().enumerate() {
        if a.command == "file.place" {
            if let Some(raw) = a.params.get("frame").and_then(Value::as_str)
                && let Some((src, "id")) = parse_step_field(raw, idx)
            {
                placed_frame_steps.insert(src);
            }
            if let Some(arr) = a.params.get("ids").and_then(Value::as_array) {
                for raw in arr.iter().filter_map(Value::as_str) {
                    if let Some((src, "id")) = parse_step_field(raw, idx) {
                        placed_frame_steps.insert(src);
                    }
                }
            }
        }
    }
    let mut paired_story_for_non_text: BTreeMap<usize, String> = BTreeMap::new();
    let mut upgrade_to_text_frames: BTreeSet<usize> = BTreeSet::new();
    for (use_idx, action) in plan.commands.iter().enumerate() {
        let Some(raw_ref) = action.params.get("story").and_then(Value::as_str) else { continue };
        let Some((source_idx, "story")) = parse_step_field(raw_ref, use_idx) else { continue };
        let Some(src_action) = plan.commands.get(source_idx) else { continue };
        if !is_non_text_item_creation(src_action) {
            continue;
        }
        if !paired_story_for_non_text.contains_key(&source_idx) {
            for m in source_idx.saturating_add(1)..use_idx {
                let Some(cand) = plan.commands.get(m) else { break };
                if is_text_frame_action(cand) {
                    paired_story_for_non_text.insert(source_idx, format!("${m}.story"));
                    break;
                }
                if is_non_text_item_creation(cand) {
                    break;
                }
            }
        }
        if !paired_story_for_non_text.contains_key(&source_idx)
            && src_action.command == "frame.create"
            && !placed_frame_steps.contains(&source_idx)
            && matches!(src_action.params.get("content").and_then(Value::as_str), None | Some("unassigned"))
            && matches!(
                action.command.as_str(),
                "story.setText"
                    | "text.insert"
                    | "text.select"
                    | "table.insert"
                    | "type.char"
                    | "type.para"
                    | "style.paragraph.apply"
                    | "style.character.apply"
                    | "story.setDirection"
            )
        {
            upgrade_to_text_frames.insert(source_idx);
        }
    }
    for idx in upgrade_to_text_frames {
        if let Some(obj) = plan.commands.get_mut(idx).and_then(|a| a.params.as_object_mut()) {
            obj.insert("content".into(), Value::String("text".into()));
            notes.push(json!({
                "kind": "commandAliasNormalized",
                "command": idx,
                "requested": "frame.create",
                "resolved": "frame.create(content=text)",
                "message": "Inferred text content type for frame.create referenced via .story by a downstream text command"
            }));
        }
    }
    let table_inserts: BTreeSet<usize> = plan.commands.iter().enumerate().filter_map(|(i, a)| (a.command == "table.insert").then_some(i)).collect();
    let id_array_producers: BTreeSet<usize> = plan
        .commands
        .iter()
        .enumerate()
        .filter_map(|(i, a)| matches!(a.command.as_str(), "transform.move" | "edit.stepAndRepeat" | "object.releaseCompoundPath").then_some(i))
        .collect();
    let null_formatting_steps: BTreeSet<usize> = plan
        .commands
        .iter()
        .enumerate()
        .filter_map(|(i, a)| {
            matches!(
                a.command.as_str(),
                "selection.set"
                    | "selection.clear"
                    | "text.select"
                    | "type.char"
                    | "type.para"
                    | "style.paragraph.create"
                    | "style.paragraph.edit"
                    | "style.paragraph.apply"
                    | "style.character.create"
                    | "style.character.edit"
                    | "style.character.apply"
                    | "style.object.create"
                    | "style.object.apply"
                    | "style.table.create"
                    | "style.table.apply"
                    | "style.cell.create"
                    | "style.cell.apply"
                    | "swatch.create"
                    | "list.define"
                    | "layout.pages.insert"
                    | "layout.documentSetup"
                    | "layout.marginsAndColumns"
                    | "object.fill"
                    | "object.stroke"
                    | "object.opacity"
                    | "object.cornerOptions"
                    | "object.effects"
                    | "object.dropShadow"
                    | "object.gradientFeather"
                    | "object.textWrap"
                    | "object.textFrameOptions"
                    | "object.fittingOptions"
                    | "object.arrange"
                    | "object.align"
                    | "object.distribute"
                    | "object.label"
                    | "object.gradient"
                    | "object.feather"
                    | "object.wrap"
                    | "object.fit"
                    | "transform.set"
                    | "transform.rotate"
                    | "transform.scale"
                    | "transform.shear"
                    | "transform.flip"
                    | "transform.clear"
                    | "story.setText"
                    | "story.setDirection"
                    | "text.insert"
                    | "text.unthread"
                    | "find.change"
                    | "table.insertRow"
                    | "table.deleteRow"
                    | "table.insertColumn"
                    | "table.deleteColumn"
                    | "table.merge"
                    | "table.unmerge"
                    | "table.setCell"
                    | "table.setRowHeight"
                    | "table.setColumnWidth"
                    | "table.distributeColumns"
                    | "table.distributeRows"
                    | "table.options"
                    | "edit.clear"
                    | "object.ungroup"
            )
            .then_some(i)
        })
        .collect();
    let non_text_creations: BTreeSet<usize> =
        plan.commands.iter().enumerate().filter_map(|(i, a)| is_non_text_item_creation(a).then_some(i)).collect();
    let mut current_item_ref: Option<String> = None;
    let mut current_story_ref: Option<String> = None;
    let mut item_ref_to_story_ref: BTreeMap<String, String> = BTreeMap::new();
    let mut story_ref_to_item_ref: BTreeMap<String, String> = BTreeMap::new();
    let mut step_item_ref: BTreeMap<usize, String> = BTreeMap::new();
    let mut step_story_ref: BTreeMap<usize, String> = BTreeMap::new();
    let mut explicit_item_steps: BTreeSet<usize> = BTreeSet::new();
    for (index, action) in plan.commands.iter_mut().enumerate() {
        let prefer_non_text_next = matches!(action.command.as_str(), "object.stroke" | "object.fill" | "object.fittingOptions");
        let resolve_item_step = |source_idx: usize, explicit_steps: &BTreeSet<usize>, step_items: &BTreeMap<usize, String>| -> Option<String> {
            if !explicit_steps.contains(&source_idx)
                && prefer_non_text_next
                && source_idx.saturating_add(1) < index
                && non_text_creations.contains(&source_idx.saturating_add(1))
            {
                Some(format!("${}.id", source_idx.saturating_add(1)))
            } else {
                step_items.get(&source_idx).cloned()
            }
        };
        if let Some(table_val) = action.params.get_mut("table")
            && let Some(raw_ref) = table_val.as_str()
            && let Some((source_idx, "id")) = parse_step_field(raw_ref, index)
            && table_inserts.contains(&source_idx)
        {
            let resolved = format!("${source_idx}.table");
            let requested = raw_ref.to_owned();
            *table_val = Value::String(resolved.clone());
            notes.push(json!({
                "kind": "commandAliasNormalized",
                "command": index,
                "requested": requested,
                "resolved": resolved,
                "message": "Normalized table.insert result reference from .id to .table"
            }));
        }
        if let Some(story_val) = action.params.get_mut("story")
            && let Some(raw_ref) = story_val.as_str()
            && let Some((source_idx, "story")) = parse_step_field(raw_ref, index)
        {
            if let Some(resolved) = paired_story_for_non_text.get(&source_idx).cloned() {
                let requested = raw_ref.to_owned();
                *story_val = Value::String(resolved.clone());
                notes.push(json!({
                    "kind": "commandAliasNormalized",
                    "command": index,
                    "requested": requested,
                    "resolved": resolved,
                    "message": "Normalized container/non-text .story reference to its paired inner text story"
                }));
            } else if null_formatting_steps.contains(&source_idx)
                && let Some(resolved) = step_story_ref.get(&source_idx).cloned()
            {
                let requested = raw_ref.to_owned();
                *story_val = Value::String(resolved.clone());
                notes.push(json!({
                    "kind": "commandAliasNormalized",
                    "command": index,
                    "requested": requested,
                    "resolved": resolved,
                    "message": "Normalized formatting/selection step .story reference to its target story producer"
                }));
            }
        }
        let item_param_keys: &[&str] = if action.command == "text.thread" { &["frame", "from", "to"] } else { &["frame"] };
        for param_key in item_param_keys {
            if let Some(frame_val) = action.params.get_mut(*param_key)
                && let Some(raw_ref) = frame_val.as_str()
                && let Some((source_idx, "id")) = parse_step_field(raw_ref, index)
                && null_formatting_steps.contains(&source_idx)
                && let Some(resolved) = resolve_item_step(source_idx, &explicit_item_steps, &step_item_ref)
            {
                let requested = raw_ref.to_owned();
                *frame_val = Value::String(resolved.clone());
                notes.push(json!({
                    "kind": "commandAliasNormalized",
                    "command": index,
                    "requested": requested,
                    "resolved": resolved,
                    "message": "Normalized formatting/selection step .id reference in item parameter to its target item producer"
                }));
            }
        }
        if let Some(ids_val) = action.params.get_mut("ids") {
            match ids_val {
                Value::Array(items) => {
                    for elem in items.iter_mut() {
                        if let Some(raw_ref) = elem.as_str()
                            && let Some((source_idx, "id")) = parse_step_field(raw_ref, index)
                            && null_formatting_steps.contains(&source_idx)
                            && let Some(resolved) = resolve_item_step(source_idx, &explicit_item_steps, &step_item_ref)
                        {
                            let requested = raw_ref.to_owned();
                            *elem = Value::String(resolved.clone());
                            notes.push(json!({
                                "kind": "commandAliasNormalized",
                                "command": index,
                                "requested": requested,
                                "resolved": resolved,
                                "message": "Normalized formatting/selection step .id reference to its target item producer"
                            }));
                        }
                    }
                }
                Value::String(raw_ref) => {
                    if let Some((source_idx, "id")) = parse_step_field(raw_ref, index)
                        && null_formatting_steps.contains(&source_idx)
                        && let Some(resolved) = resolve_item_step(source_idx, &explicit_item_steps, &step_item_ref)
                    {
                        let requested = raw_ref.clone();
                        *raw_ref = resolved.clone();
                        notes.push(json!({
                            "kind": "commandAliasNormalized",
                            "command": index,
                            "requested": requested,
                            "resolved": resolved,
                            "message": "Normalized formatting/selection step .id reference to its target item producer"
                        }));
                    }
                }
                _ => {}
            }
            let mut replacement: Option<(String, Value, &'static str)> = None;
            match ids_val {
                Value::Array(items) if items.len() == 1 => {
                    if let Some(raw) = items.first().and_then(Value::as_str) {
                        let body = raw.strip_prefix("${").and_then(|b| b.strip_suffix('}')).or_else(|| raw.strip_prefix('$'));
                        if let Some((step_str, field)) = body.and_then(|b| b.split_once('.')) {
                            if field == "ids" {
                                replacement = Some((
                                    format!("[\"{raw}\"]"),
                                    Value::String(raw.to_owned()),
                                    "Unwrapped ItemId[] result reference from nested ids array",
                                ));
                            } else if field == "id" {
                                let resolved_step = if step_str == "last" { index.checked_sub(1) } else { step_str.parse::<usize>().ok() };
                                if let Some(src) = resolved_step.filter(|&s| s < index && id_array_producers.contains(&s)) {
                                    let resolved = if raw.starts_with("${") { format!("${{{step_str}.ids}}") } else { format!("${step_str}.ids") };
                                    let _ = src;
                                    replacement = Some((
                                        format!("[\"{raw}\"]"),
                                        Value::String(resolved),
                                        "Normalized array-producing command reference from .id to .ids",
                                    ));
                                }
                            }
                        }
                    }
                }
                Value::String(raw) => {
                    let body = raw.strip_prefix("${").and_then(|b| b.strip_suffix('}')).or_else(|| raw.strip_prefix('$'));
                    if let Some((step_str, field)) = body.and_then(|b| b.split_once('.')) {
                        if field == "id" {
                            let resolved_step = if step_str == "last" { index.checked_sub(1) } else { step_str.parse::<usize>().ok() };
                            if resolved_step.is_some_and(|s| s < index && id_array_producers.contains(&s)) {
                                let resolved = if raw.starts_with("${") { format!("${{{step_str}.ids}}") } else { format!("${step_str}.ids") };
                                replacement =
                                    Some((raw.clone(), Value::String(resolved), "Normalized array-producing command reference from .id to .ids"));
                            } else {
                                replacement = Some((raw.clone(), json!([raw.clone()]), "Wrapped scalar ItemId reference in ids array"));
                            }
                        } else if field != "ids" {
                            replacement = Some((raw.clone(), json!([raw.clone()]), "Wrapped scalar element reference in ids array"));
                        }
                    }
                }
                _ => {}
            }
            if let Some((requested, resolved_val, message)) = replacement {
                let resolved_desc = resolved_val.to_string();
                *ids_val = resolved_val;
                notes.push(json!({
                    "kind": "commandAliasNormalized",
                    "command": index,
                    "requested": requested,
                    "resolved": resolved_desc,
                    "message": message
                }));
            }
        }
        match action.command.as_str() {
            "frame.create" => {
                let item_r = format!("${index}.id");
                current_item_ref = Some(item_r.clone());
                let is_text = is_text_frame_action(action);
                if is_text {
                    let story_r = format!("${index}.story");
                    item_ref_to_story_ref.insert(item_r.clone(), story_r.clone());
                    story_ref_to_item_ref.insert(story_r.clone(), item_r);
                    current_story_ref = Some(story_r);
                } else {
                    current_story_ref = None;
                }
            }
            "shape.create"
            | "line.create"
            | "path.create"
            | "object.qrCode"
            | "object.group"
            | "object.makeCompoundPath"
            | "object.convertShape"
            | "object.pathfinder" => {
                current_item_ref = Some(format!("${index}.id"));
                current_story_ref = None;
            }
            "edit.stepAndRepeat" | "object.releaseCompoundPath" | "selection.clear" => {
                current_item_ref = None;
                current_story_ref = None;
            }
            "edit.clear" | "object.ungroup" => {
                let mut invalidated_items = Vec::new();
                if let Some(arr) = action.params.get("ids").and_then(Value::as_array) {
                    for raw in arr.iter().filter_map(Value::as_str) {
                        if let Some((src, "id")) = parse_step_field(raw, index) {
                            invalidated_items.push(format!("${src}.id"));
                        } else if raw.starts_with('$') {
                            invalidated_items.push(raw.to_owned());
                        }
                    }
                } else if let Some(cur) = current_item_ref.take() {
                    invalidated_items.push(cur);
                }
                current_item_ref = None;
                current_story_ref = None;
                for dead_item in invalidated_items {
                    let dead_story = item_ref_to_story_ref.remove(&dead_item);
                    if let Some(ds) = &dead_story {
                        story_ref_to_item_ref.remove(ds);
                        step_story_ref.retain(|_, v| v != ds);
                    }
                    step_item_ref.retain(|_, v| v != &dead_item);
                }
            }
            "selection.set" => {
                if let Some(arr) = action.params.get("ids").and_then(Value::as_array)
                    && arr.len() == 1
                    && let Some(id_str) = arr.first().and_then(Value::as_str)
                    && id_str.starts_with('$')
                {
                    current_item_ref = Some(id_str.to_owned());
                    step_item_ref.insert(index, id_str.to_owned());
                    explicit_item_steps.insert(index);
                    current_story_ref = item_ref_to_story_ref.get(id_str).cloned();
                    if let Some(sr) = &current_story_ref {
                        step_story_ref.insert(index, sr.clone());
                    }
                } else {
                    current_item_ref = None;
                    current_story_ref = None;
                }
            }
            "text.select" | "story.setText" | "story.setDirection" => {
                if let Some(sr) = action.params.get("story").and_then(Value::as_str)
                    && sr.starts_with('$')
                {
                    current_story_ref = Some(sr.to_owned());
                    step_story_ref.insert(index, sr.to_owned());
                    if let Some(ir) = story_ref_to_item_ref.get(sr).cloned() {
                        current_item_ref = Some(ir.clone());
                        step_item_ref.insert(index, ir);
                        explicit_item_steps.insert(index);
                    }
                }
            }
            "type.char"
            | "type.para"
            | "style.paragraph.apply"
            | "style.character.apply"
            | "text.insert"
            | "find.change"
            | "style.table.apply"
            | "style.cell.create"
            | "style.cell.apply" => {
                if let Some(sr) = action.params.get("story").and_then(Value::as_str)
                    && sr.starts_with('$')
                {
                    step_story_ref.insert(index, sr.to_owned());
                    if let Some(ir) = story_ref_to_item_ref.get(sr).cloned() {
                        step_item_ref.insert(index, ir);
                        explicit_item_steps.insert(index);
                    }
                } else if matches!(
                    action.command.as_str(),
                    "type.char" | "type.para" | "style.paragraph.apply" | "style.character.apply" | "text.insert"
                ) {
                    if let Some(ir) = &current_item_ref {
                        step_item_ref.insert(index, ir.clone());
                    }
                    if let Some(sr) = &current_story_ref {
                        step_story_ref.insert(index, sr.clone());
                    }
                }
            }
            "object.fill"
            | "object.stroke"
            | "object.opacity"
            | "object.cornerOptions"
            | "object.effects"
            | "object.dropShadow"
            | "object.gradientFeather"
            | "object.textWrap"
            | "object.textFrameOptions"
            | "object.fittingOptions"
            | "object.arrange"
            | "object.align"
            | "object.distribute"
            | "object.label"
            | "object.gradient"
            | "object.feather"
            | "object.wrap"
            | "object.fit"
            | "transform.set"
            | "transform.rotate"
            | "transform.scale"
            | "transform.shear"
            | "transform.flip"
            | "transform.clear"
            | "style.object.apply" => {
                if let Some(arr) = action.params.get("ids").and_then(Value::as_array)
                    && arr.len() == 1
                    && let Some(id_str) = arr.first().and_then(Value::as_str)
                    && id_str.starts_with('$')
                {
                    step_item_ref.insert(index, id_str.to_owned());
                    explicit_item_steps.insert(index);
                    if let Some(sr) = item_ref_to_story_ref.get(id_str) {
                        step_story_ref.insert(index, sr.clone());
                    }
                } else if action.params.get("ids").is_none() {
                    if let Some(ir) = &current_item_ref {
                        step_item_ref.insert(index, ir.clone());
                    }
                    if let Some(sr) = &current_story_ref {
                        step_story_ref.insert(index, sr.clone());
                    }
                }
            }
            _ => {}
        }
    }
    notes
}

/// A generation plan has already supplied its complete copy. A start caret
/// followed by character formatting would otherwise style only future typing,
/// leaving the generated words at the engine defaults. Recover this narrow
/// no-op using a real UTF-8 selection, never changing general caret semantics.
fn format_populated_new_stories(plan: &mut ProductionPlan) -> Result<Vec<Value>, String> {
    // Editing plans may intentionally set insertion formatting before replacing
    // or inserting copy. Their cursor/range contract stays untouched.
    if plan.commands.iter().any(|action| matches!(action.command.as_str(), "text.insert" | "text.delete" | "text.replace" | "story.setText")) {
        return Ok(Vec::new());
    }
    let mut notes = Vec::new();
    for index in 0..plan.commands.len() {
        let Some(action) = plan.commands.get(index) else { continue };
        if action.command != "text.select"
            || action.params.get("anchor").and_then(Value::as_u64) != Some(0)
            || action.params.get("focus").and_then(Value::as_u64) != Some(0)
            || plan.commands.get(index.saturating_add(1)).is_none_or(|next| next.command != "type.char")
        {
            continue;
        }
        let Some(source) = action
            .params
            .get("story")
            .and_then(Value::as_str)
            .and_then(|reference| reference.strip_prefix('$')?.strip_suffix(".story")?.parse::<usize>().ok())
        else {
            continue;
        };
        if source >= index {
            continue;
        }
        let earlier = plan.commands.get(source.saturating_add(1)..index).ok_or("Invalid generated formatting range")?;
        let story_reference = format!("${source}.story");
        let frame_reference = format!("${source}.id");
        if earlier.iter().any(|previous| {
            (previous.command == "text.select" && previous.params.get("story").and_then(Value::as_str) == Some(story_reference.as_str()))
                || (previous.command == "selection.set"
                    && previous
                        .params
                        .get("ids")
                        .and_then(Value::as_array)
                        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(frame_reference.as_str()))))
        }) {
            // An earlier explicit range/frame selection may carry deliberately
            // mixed typography. Do not reinterpret its later insertion caret.
            continue;
        }
        let Some(frame) = plan
            .commands
            .get(source)
            .filter(|frame| frame.command == "frame.create" && frame.params.get("content").and_then(Value::as_str) == Some("text"))
        else {
            continue;
        };
        let Some(copy) = frame.params.get("text").and_then(Value::as_str).filter(|copy| !copy.is_empty()) else { continue };
        let bytes = copy.len();
        let selection = plan.commands.get_mut(index).ok_or("Generated formatting selection disappeared")?;
        *selection.params.get_mut("focus").ok_or("Generated formatting selection has no focus")? = json!(bytes);
        notes.push(json!({"kind":"nativeFormattingRange","command":index,"frame":source,"utf8_bytes":bytes,
            "message":"Applied generated character formatting to the complete newly supplied copy, using a native UTF-8 text selection"}));
    }
    Ok(notes)
}

/// Resolve bare special swatch names (`Paper` -> `[Paper]`), inline color literals
/// (`C=0 M=15 Y=95 K=0`, `C:85 M:75 Y:0 K:0`, `R=... G=... B=...`, `#RRGGBB`, `#RGB`),
/// and semantic palette color names to existing document swatches when page-isolated
/// production cannot call `swatch.create`.
pub(crate) fn recover_unregistered_swatches(plan: &mut ProductionPlan, base: &DocState) -> Result<Vec<Value>, String> {
    let mut known: BTreeSet<String> = base.doc.swatches.iter().map(|s| s.name.clone()).collect();
    for action in &plan.commands {
        if action.command == "swatch.create" {
            if let Some(name) = action.params.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                known.insert(name.to_owned());
            } else if let Some(color) = action.params.get("color").and_then(Value::as_object)
                && let (Some(c), Some(m), Some(y), Some(k)) = (
                    color.get("c").and_then(Value::as_f64),
                    color.get("m").and_then(Value::as_f64),
                    color.get("y").and_then(Value::as_f64),
                    color.get("k").and_then(Value::as_f64),
                )
            {
                known.insert(designcraft_engine::doc::color::swatch::cmyk_name(
                    (c / 100.0) as f32,
                    (m / 100.0) as f32,
                    (y / 100.0) as f32,
                    (k / 100.0) as f32,
                ));
            }
        }
    }
    let bare = |name: &str| name.trim().trim_start_matches('[').trim_end_matches(']').to_lowercase();
    let resolve_swatch = |requested: &str| -> Option<String> {
        if requested.is_empty() || requested.starts_with('$') || known.contains(requested) {
            return None;
        }
        let requested_bare = bare(requested);
        if let Some(exact) = base.doc.swatches.iter().find(|s| bare(&s.name) == requested_bare) {
            Some(exact.name.clone())
        } else if requested_bare == "transparent"
            && let Some(none_swatch) = base.doc.swatches.iter().find(|s| matches!(s.value, designcraft_engine::doc::color::SwatchValue::None))
        {
            Some(none_swatch.name.clone())
        } else if let Some(target) = parse_inline_color(requested) {
            let target_rgb = target.to_rgb_uncalibrated();
            base.doc
                .swatches
                .iter()
                .filter(|s| {
                    !matches!(s.value, designcraft_engine::doc::color::SwatchValue::None | designcraft_engine::doc::color::SwatchValue::Registration)
                })
                .filter_map(|s| {
                    let color = designcraft_engine::doc::color::swatch::resolve(&base.doc.swatches, &s.name, 1.0)?;
                    let rgb = color.to_rgb_uncalibrated();
                    let dist = (rgb[0] - target_rgb[0]).powi(2) + (rgb[1] - target_rgb[1]).powi(2) + (rgb[2] - target_rgb[2]).powi(2);
                    Some((s.name.clone(), dist))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(name, _)| name)
        } else {
            None
        }
    };
    let mut notes = Vec::new();
    let mut check_slot = |index: usize, slot: Option<&mut Value>| {
        let Some(slot) = slot else { return };
        let Some(requested) = slot.as_str().map(str::to_owned) else { return };
        if let Some(resolved) = resolve_swatch(&requested) {
            *slot = Value::String(resolved.clone());
            notes.push(json!({
                "kind": "nativeSwatchFallback",
                "command": index,
                "requested": requested,
                "resolved": resolved,
                "message": "Resolved unregistered swatch reference to an existing document swatch"
            }));
        }
    };
    for (index, action) in plan.commands.iter_mut().enumerate() {
        match action.command.as_str() {
            "object.fill" | "object.stroke" => check_slot(index, action.params.get_mut("swatch")),
            "object.dropShadow" | "object.qrCode" => check_slot(index, action.params.get_mut("color")),
            "object.gradient" => {
                if let Some(stops) = action.params.get_mut("stops").and_then(Value::as_array_mut) {
                    for stop in stops {
                        check_slot(index, stop.get_mut("swatch"));
                    }
                }
            }
            "type.char" => check_slot(index, action.params.get_mut("attrs").and_then(|attrs| attrs.get_mut("fill"))),
            "type.para" => {
                if let Some(attrs) = action.params.get_mut("attrs") {
                    check_slot(index, attrs.get_mut("shadingColor"));
                    check_slot(index, attrs.get_mut("borderColor"));
                }
            }
            "style.paragraph.create" | "style.paragraph.edit" | "style.character.create" | "style.character.edit" => {
                check_slot(index, action.params.get_mut("chars").and_then(|chars| chars.get_mut("fill")));
                if let Some(para) = action.params.get_mut("para") {
                    check_slot(index, para.get_mut("shadingColor"));
                    check_slot(index, para.get_mut("borderColor"));
                }
            }
            "style.object.create" => {
                check_slot(index, action.params.get_mut("fill"));
                check_slot(index, action.params.get_mut("dropShadow").and_then(|s| s.get_mut("color")));
                if let Some(effects) = action.params.get_mut("effects") {
                    for key in ["dropShadow", "innerShadow", "outerGlow", "innerGlow", "satin"] {
                        check_slot(index, effects.get_mut(key).and_then(|e| e.get_mut("color")));
                    }
                    if let Some(bevel) = effects.get_mut("bevel") {
                        check_slot(index, bevel.get_mut("highlight"));
                        check_slot(index, bevel.get_mut("shadow"));
                    }
                }
            }
            "object.effects" => {
                if let Some(effects) = action.params.get_mut("effects") {
                    for key in ["dropShadow", "innerShadow", "outerGlow", "innerGlow", "satin"] {
                        check_slot(index, effects.get_mut(key).and_then(|e| e.get_mut("color")));
                    }
                    if let Some(bevel) = effects.get_mut("bevel") {
                        check_slot(index, bevel.get_mut("highlight"));
                        check_slot(index, bevel.get_mut("shadow"));
                    }
                }
            }
            "style.cell.create" => {
                check_slot(index, action.params.get_mut("fill"));
                check_slot(index, action.params.get_mut("stroke").and_then(|s| s.get_mut("color")));
            }
            "style.table.create" | "table.options" => {
                check_slot(index, action.params.get_mut("border").and_then(|b| b.get_mut("color")));
                for key in ["altRows", "altCols"] {
                    if let Some(alt) = action.params.get_mut(key) {
                        check_slot(index, alt.get_mut("firstColor"));
                        check_slot(index, alt.get_mut("nextColor"));
                    }
                }
            }
            "table.setCell" => check_slot(index, action.params.get_mut("fill")),
            _ => {}
        }
    }
    Ok(notes)
}

fn normalize_unit_interval_fields(plan: &mut ProductionPlan) -> Vec<Value> {
    let mut notes = Vec::new();
    let mut norm_slot = |index: usize, cmd: &str, field: &str, slot: Option<&mut Value>| {
        let Some(slot) = slot else { return };
        let Some(raw) = slot.as_f64().filter(|v| v.is_finite() && *v > 1.0 && *v <= 100.0) else {
            return;
        };
        let normalized = raw / 100.0;
        *slot = json!(normalized);
        notes.push(json!({
            "kind": "unitIntervalNormalized",
            "command": index,
            "field": format!("{cmd}.{field}"),
            "requested": raw,
            "resolved": normalized,
            "message": "Normalized percentage unit-interval value into [0.0, 1.0]"
        }));
    };
    for (index, action) in plan.commands.iter_mut().enumerate() {
        let cmd = action.command.clone();
        match cmd.as_str() {
            "object.opacity" | "object.dropShadow" => norm_slot(index, &cmd, "opacity", action.params.get_mut("opacity")),
            "object.fill" | "object.stroke" | "table.setCell" => norm_slot(index, &cmd, "tint", action.params.get_mut("tint")),
            "style.cell.create" => {
                norm_slot(index, &cmd, "tint", action.params.get_mut("tint"));
                norm_slot(index, &cmd, "stroke.tint", action.params.get_mut("stroke").and_then(|s| s.get_mut("tint")));
            }
            "style.table.create" | "table.options" => {
                norm_slot(index, &cmd, "border.tint", action.params.get_mut("border").and_then(|b| b.get_mut("tint")));
                for key in ["altRows", "altCols"] {
                    if let Some(alt) = action.params.get_mut(key) {
                        norm_slot(index, &cmd, &format!("{key}.firstTint"), alt.get_mut("firstTint"));
                        norm_slot(index, &cmd, &format!("{key}.nextTint"), alt.get_mut("nextTint"));
                    }
                }
            }
            "object.effects" | "style.object.create" => {
                norm_slot(index, &cmd, "opacity", action.params.get_mut("opacity"));
                norm_slot(index, &cmd, "dropShadow.opacity", action.params.get_mut("dropShadow").and_then(|s| s.get_mut("opacity")));
                if let Some(effects) = action.params.get_mut("effects") {
                    for key in ["dropShadow", "innerShadow", "outerGlow", "innerGlow", "satin"] {
                        norm_slot(index, &cmd, &format!("effects.{key}.opacity"), effects.get_mut(key).and_then(|e| e.get_mut("opacity")));
                    }
                    if let Some(bevel) = effects.get_mut("bevel") {
                        norm_slot(index, &cmd, "effects.bevel.highlightOpacity", bevel.get_mut("highlightOpacity"));
                        norm_slot(index, &cmd, "effects.bevel.shadowOpacity", bevel.get_mut("shadowOpacity"));
                    }
                }
            }
            _ => {}
        }
    }
    notes
}

fn parse_keyed_channels<const N: usize>(raw: &str, keys: [char; N], max: f32) -> Option<[f32; N]> {
    let mut normalized = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == ',' || ch == ';' {
            normalized.push(' ');
        } else if ch == '=' || ch == ':' {
            normalized.push('=');
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
        } else {
            normalized.push(ch);
        }
    }
    let parts: Vec<&str> = normalized.split_whitespace().collect();
    if parts.len() != N {
        return None;
    }
    let mut out = [0.0; N];
    for (slot, (key, part)) in out.iter_mut().zip(keys.into_iter().zip(parts.iter())) {
        let rest = part.strip_prefix(key).or_else(|| part.strip_prefix(key.to_ascii_lowercase()))?.strip_prefix('=')?.trim_end_matches('%');
        let value = rest.parse::<f32>().ok()?;
        if !value.is_finite() || !(0.0..=max).contains(&value) {
            return None;
        }
        *slot = value / max;
    }
    Some(out)
}

fn parse_inline_color(raw: &str) -> Option<designcraft_engine::doc::color::Color> {
    use designcraft_engine::doc::color::Color;
    let trimmed = raw.trim();
    if let Some(hex) = trimmed.strip_prefix('#').filter(|h| h.is_ascii()) {
        if hex.len() == 6
            && let (Some(rs), Some(gs), Some(bs)) = (hex.get(0..2), hex.get(2..4), hex.get(4..6))
            && let (Ok(r), Ok(g), Ok(b)) = (u8::from_str_radix(rs, 16), u8::from_str_radix(gs, 16), u8::from_str_radix(bs, 16))
        {
            return Some(Color::rgb8(r, g, b));
        }
        if hex.len() == 3
            && let (Some(rs), Some(gs), Some(bs)) = (hex.get(0..1), hex.get(1..2), hex.get(2..3))
            && let (Ok(r), Ok(g), Ok(b)) = (u8::from_str_radix(rs, 16), u8::from_str_radix(gs, 16), u8::from_str_radix(bs, 16))
        {
            return Some(Color::rgb8(r * 17, g * 17, b * 17));
        }
    }
    if let Some([c, m, y, k]) = parse_keyed_channels(trimmed, ['C', 'M', 'Y', 'K'], 100.0) {
        return Some(Color::cmyk(c, m, y, k));
    }
    if let Some([r, g, b]) = parse_keyed_channels(trimmed, ['R', 'G', 'B'], 255.0) {
        return Some(Color::rgb(r, g, b));
    }
    let normalized: String = trimmed
        .trim_start_matches('[')
        .trim_end_matches(']')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match normalized.as_str() {
        "white" | "paper" | "warm cream" | "cream" | "ivory" => Some(Color::WHITE),
        "black" | "charcoal" | "ink" | "dark" => Some(Color::cmyk(0.0, 0.0, 0.0, 1.0)),
        "gray" | "grey" | "silver" | "slate" => Some(Color::cmyk(0.0, 0.0, 0.0, 0.5)),
        "sunshine yellow" | "yellow" | "gold" | "amber" | "lemon" => Some(Color::cmyk(0.0, 0.15, 0.95, 0.0)),
        "emerald green" | "emerald" | "green" | "mint" | "lime" | "sage" => Some(Color::cmyk(0.75, 0.10, 0.85, 0.0)),
        "berry purple" | "purple" | "violet" | "plum" | "lavender" => Some(Color::cmyk(0.45, 0.85, 0.0, 0.0)),
        "cosmic indigo" | "indigo" | "navy" | "blue" | "royal blue" => Some(Color::cmyk(0.85, 0.75, 0.0, 0.0)),
        "sunny tangerine" | "tangerine" | "orange" | "coral" | "peach" => Some(Color::cmyk(0.0, 0.50, 0.95, 0.0)),
        "sky cyan" | "sky blue" | "cyan" | "teal" | "aqua" | "turquoise" => Some(Color::cmyk(0.65, 0.10, 0.0, 0.0)),
        "red" | "crimson" | "scarlet" => Some(Color::cmyk(0.0, 1.0, 1.0, 0.0)),
        "magenta" | "pink" | "rose" => Some(Color::cmyk(0.0, 1.0, 0.0, 0.0)),
        _ => None,
    }
}

fn check_repeated_slots(plan: &ProductionPlan, state: &DocState, cancel: &AtomicBool) -> Result<(), String> {
    for (index, image) in plan.images.iter().enumerate().filter(|(_, image)| image.instance_count() > 1) {
        let obstacles = fitting_obstacles(&state.doc, image.page, cancel)?;
        for r in image.all_rects() {
            let bounds = Rect::new(r[0], r[1], r[2], r[3]);
            if plan.commands.iter().any(|a| {
                a.command == "frame.create"
                    && a.params.get("content").and_then(Value::as_str) == Some("graphic")
                    && a.params.get("spread").and_then(Value::as_u64).unwrap_or(0) == image.page as u64
                    && a.params.get("rect").is_some_and(|v| *v == serde_json::json!(r))
            }) {
                return Err("Do not precreate repeated graphic cells; the host duplicates the source frame".into());
            }
            for obstacle in &obstacles {
                let Some(item) = state.doc.item(obstacle.id) else { continue };
                if item.text_frame().is_some() && overlaps(bounds, obstacle.bounds) {
                    return Err("Repeated illustration cells overlap native text; reflow the cells before image generation".into());
                }
                if item.graphic().is_some() && overlaps(bounds, obstacle.bounds) {
                    return Err("Repeated illustration cells overlap existing artwork; no images were requested".into());
                }
            }
            if plan.images.iter().enumerate().any(|(other_index, other)| {
                other_index != index && other.page == image.page && other.all_rects().any(|other| crate::image_repetition::overlaps(r, other))
            }) {
                return Err("Repeated illustration cells overlap another pending image; no images were requested".into());
            }
        }
    }
    Ok(())
}

fn frame_identity(spread: usize, index: usize, id: ItemId, rect: Rect, text: &str) -> String {
    let mut copy: String =
        text.chars().take(80).map(|character| if character.is_whitespace() || character.is_control() { ' ' } else { character }).collect();
    if text.chars().nth(80).is_some() {
        copy.push('…');
    }
    format!(
        "Page {} native text item {} (source ${index}, rect [{:.2}, {:.2}, {:.2}, {:.2}], copy {copy:?})",
        spread + 1,
        id.0,
        rect.x0,
        rect.y0,
        rect.x1,
        rect.y1
    )
}

struct FittingObstacle {
    id: ItemId,
    bounds: Rect,
    inherited: bool,
}

struct BackgroundGrowth {
    index: usize,
    id: ItemId,
    original: Rect,
    target: Rect,
}

fn is_expandable_card_shape(item: &designcraft_engine::doc::Item) -> bool {
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

fn is_allowed_fit_location(doc: &designcraft_engine::doc::Document, loc: &designcraft_engine::doc::ItemLoc) -> bool {
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
fn is_snug_growable_background(
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
fn containing_background_growths(
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

fn background_growth_blocks(background: &BackgroundGrowth, old: Rect, target: Rect, band: Rect) -> bool {
    if contains(old, background.original) {
        !contains(target, background.target)
    } else if contains(background.original, old) {
        !contains(background.target, target)
    } else {
        overlaps(band, target)
    }
}

fn fitting_obstacles(doc: &designcraft_engine::doc::Document, spread_index: usize, cancel: &AtomicBool) -> Result<Vec<FittingObstacle>, String> {
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
    frame_refs: &BTreeMap<ItemId, (usize, String, String)>,
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
    // Only tiny stacked headers and unambiguous bilingual header columns may
    // move. Body activities and mixed-language frames are never guessed.
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
        if first.language != 0
            && first.language == second.language
            && (first.target.x0 - second.target.x0).abs() < 0.01
            && (first.target.x1 - second.target.x1).abs() < 0.01
        {
            let (upper, mut lower, lower_index) = if first.target.y0 < second.target.y0 { (first, second, b) } else { (second, first, a) };
            let overlap = upper.target.y1 - lower.target.y0;
            let header_bottom = lower.safe.y0 + (lower.safe.height() * 0.25).min(72.0);
            if upper.target.y0 + 0.01 < lower.target.y0
                && upper.target.y1 + 0.01 < lower.target.y1
                && overlap > 0.0
                && overlap <= 12.0
                && upper.target.y1 <= header_bottom
                && lower.target.y1 + overlap <= header_bottom
            {
                lower.target = shift(lower.target, 0.0, overlap);
                let mut targets: BTreeMap<_, _> = positions.iter().map(|position| (position.id, position.target)).collect();
                targets.insert(lower.id, lower.target);
                // Check before changing another position: an activity immediately
                // below this pair must remain an obstruction, never a reflow chain.
                if !contains(lower.safe, lower.target) || position_collision(&lower, &targets, &doc, plan, cancel)? {
                    return Err("Stacked native headers have no safe free space for a minimal downward translation; copy, fonts and existing artwork were preserved".into());
                }
                positions.get_mut(lower_index).ok_or("Missing lower header position")?.target = lower.target;
                continue;
            }
            return Err("Overlapping native text is not a small unambiguous stack within the unit header; reflow the layout".into());
        }
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
        let item_ref = frame_refs.get(&position.id).map(|(_, r, _)| r.clone()).unwrap_or_else(|| format!("${}.id", position.index));
        scratch.execute("transform.move", &json!({"ids":[position.id.0],"dx":dx,"dy":dy})).map_err(|error| error.to_string())?;
        plan.commands.push(EngineAction { command: "transform.move".into(), params: json!({"ids":[item_ref],"dx":dx,"dy":dy}) });
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
    for [x0, y0, x1, y1] in plan.images.iter().filter(|image| image.page == position.spread).flat_map(|image| image.all_rects()) {
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

    #[test]
    fn ai24_formatting_range_recovery_is_utf8_exact_idempotent_and_preserves_editing_ranges() {
        let copy = "الأعداد ١٥ و١٦";
        let base: ProductionPlan = serde_json::from_value(json!({"title":"Arabic header","images":[],"commands":[
            {"command":"frame.create","params":{"spread":0,"content":"text","rect":[50,50,400,100],"text":copy}},
            {"command":"text.select","params":{"story":"$0.story","anchor":0,"focus":0}},
            {"command":"type.char","params":{"attrs":{"size":15}}}
        ]}))
        .unwrap();
        let mut recovered = base.clone();
        assert_eq!(format_populated_new_stories(&mut recovered).unwrap().len(), 1);
        assert_eq!(recovered.commands[1].params["focus"], copy.len());
        assert!(format_populated_new_stories(&mut recovered).unwrap().is_empty());
        assert_eq!(recovered.commands[0].params["text"], copy);
        for change in ["insert", "replace", "external", "midstory", "range", "prior_range"] {
            let mut plan = base.clone();
            match change {
                "insert" => plan.commands.push(EngineAction { command: "text.insert".into(), params: json!({"text":"A"}) }),
                "replace" => plan.commands.push(EngineAction { command: "story.setText".into(), params: json!({"story":"$0.story","text":"B"}) }),
                "external" => plan.commands[1].params["story"] = json!(123),
                "midstory" => {
                    plan.commands[1].params["anchor"] = json!(2);
                    plan.commands[1].params["focus"] = json!(2);
                }
                "prior_range" => {
                    plan.commands.insert(1, EngineAction { command: "text.select".into(), params: json!({"story":"$0.story","anchor":0,"focus":2}) });
                }
                _ => plan.commands[1].params["focus"] = json!(2),
            }
            let before = serde_json::to_value(&plan).unwrap();
            assert!(format_populated_new_stories(&mut plan).unwrap().is_empty(), "{change}");
            assert_eq!(serde_json::to_value(&plan).unwrap(), before, "{change}");
        }
    }

    #[test]
    fn ai24_new_story_caret_formatting_targets_existing_copy_without_changing_engine_caret_semantics() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
        let mut plan: ProductionPlan = serde_json::from_value(json!({"title":"Header formatting","images":[],"commands":[
            {"command":"frame.create","params":{"spread":0,"content":"text","rect":[54,76,280,110],"text":"Numbers 15 & 16: Ocean Treasure Counting"}},
            {"command":"text.select","params":{"story":"$0.story","anchor":0,"focus":0}},
            {"command":"type.char","params":{"attrs":{"size":9.5}}}
        ]})).unwrap();
        let notes = prepare(&mut plan, session.doc().unwrap(), &session.prefs, &AtomicBool::new(false)).unwrap();
        assert_eq!(plan.commands[1].params["focus"], plan.commands[0].params["text"].as_str().unwrap().len());
        assert!(notes.iter().any(|note| note["kind"] == "nativeFormattingRange"));
        let result =
            session.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
        let story = session.doc().unwrap().doc.story(StoryId(result["results"][0]["story"].as_u64().unwrap())).unwrap();
        let doc = &session.doc().unwrap().doc;
        let (_, base) = doc.styles.resolve_para(&story.paras[0]);
        for byte in [0, story.text.len() - 1] {
            assert_eq!(doc.styles.resolve_char(&base, story.char_format_of(byte)).size, 9.5);
        }
        // The general engine retains insertion-only semantics at a real caret.
        session.execute("text.select", &json!({"story":story.id.0,"anchor":0,"focus":0})).unwrap();
        session.execute("type.char", &json!({"attrs":{"size":30}})).unwrap();
        let doc = &session.doc().unwrap().doc;
        let story = doc.story(StoryId(result["results"][0]["story"].as_u64().unwrap())).unwrap();
        assert_eq!(doc.styles.resolve_char(&base, story.char_format_of(0)).size, 9.5);
    }

    #[test]
    fn ai24_each_repeated_cell_is_a_pre_image_margin_and_text_obstacle() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":1,"facingPages":false})).unwrap();
        for (cell, expected) in [([220.0, 180.0, 260.0, 220.0], "overlap native text"), ([1.0, 180.0, 41.0, 220.0], "safe margin")] {
            let mut plan: ProductionPlan = serde_json::from_value(json!({"title":"Units","commands":[
                {"command":"frame.create","params":{"spread":0,"rect":[215,180,360,225],"content":"text","text":"Keep this copy"}}
            ],"images":[{"prompt":"One fish","page":0,"rect":[60,180,100,220],"label":"Fish","repeat_rects":[cell]}]}))
            .unwrap();
            let before = serde_json::to_value(&plan).unwrap();
            let error = prepare(&mut plan, session.doc().unwrap(), &session.prefs, &AtomicBool::new(false)).unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert_eq!(serde_json::to_value(&plan).unwrap(), before);
            assert!(session.doc().unwrap().doc.spreads[0].items.is_empty());
        }
    }
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

    fn contained_growth_fixture() -> (Session, ProductionPlan, Rect) {
        let (session, mut plan) = fixture();
        plan.commands.insert(2, EngineAction { command: "object.textFrameOptions".into(), params: json!({"ids":["$0.id"],"inset":[2,3,2,3]}) });
        let mut measured = Session::new();
        measured.prefs = session.prefs.clone();
        measured.add_document(session.doc().unwrap().clone());
        let applied =
            measured.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":false})).unwrap();
        let id = applied["results"][0]["id"].as_u64().unwrap();
        measured.execute("object.fit", &json!({"ids":[id],"mode":"fitFrameToContent","axis":"height"})).unwrap();
        let fitted = measured.doc().unwrap().doc.item(ItemId(id)).unwrap().bounds();
        let old_bottom = fitted.y1 - 4.4;
        plan.commands[0].params["rect"][3] = json!(old_bottom);
        let background = Rect::new(54.0, 54.0, 231.0, old_bottom + 1.0);
        plan.commands.insert(0, EngineAction { command: "frame.create".into(), params: json!({"spread":0,"content":"unassigned","shape":"rectangle","rect":[background.x0,background.y0,background.x1,background.y1]}) });
        plan.commands[3].params["ids"] = json!(["$1.id"]);
        (session, plan, background)
    }

    #[test]
    fn ai21_new_decorative_container_grows_with_tiny_text_reflow_and_keeps_padding_and_insets() {
        let (session, mut plan, old_background) = contained_growth_fixture();
        let authored = plan.clone();
        let base = session.doc().unwrap().clone();
        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(notes.iter().any(|note| note["kind"] == "nativeBackgroundReflow"));
        assert_eq!(plan.commands[1].params["text"], authored.commands[1].params["text"]);
        assert_eq!(plan.commands[2].params, authored.commands[2].params, "Font size and leading must stay authored");
        assert_eq!(plan.commands[3].params, authored.commands[3].params, "Explicit text insets must never change");
        let mut proof = Session::new();
        proof.prefs = session.prefs.clone();
        proof.add_document(base.clone());
        let result =
            proof.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
        let doc = &proof.doc().unwrap().doc;
        let background = doc.item(ItemId(result["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
        let text = doc.item(ItemId(result["results"][1]["id"].as_u64().unwrap())).unwrap().bounds();
        assert!((background.y1 - text.y1 - 1.0).abs() < 0.02, "Bottom padding must survive local reflow");
        assert!(background.y1 > old_background.y1 && background.y1 - old_background.y1 < 5.0);
        assert_eq!((background.x0, background.y0, background.x1), (old_background.x0, old_background.y0, old_background.x1));
        assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
    }

    #[test]
    fn ai21_container_growth_refuses_real_and_inherited_obstacles_in_its_padding_band() {
        for inherited in [false, true] {
            let (mut session, mut plan, background) = contained_growth_fixture();
            let obstacle = json!({"spread":if inherited {json!({"kind":"parent","index":0})} else {json!(0)},
                "content":"graphic","rect":[54,background.y1+1.0,59,background.y1+3.0]});
            if inherited {
                session.execute("frame.create", &obstacle).unwrap();
            } else {
                plan.commands.push(EngineAction { command: "frame.create".into(), params: obstacle });
            }
            let authored = serde_json::to_value(&plan).unwrap();
            let base = session.doc().unwrap().clone();
            assert!(prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).is_err());
            assert_eq!(serde_json::to_value(&plan).unwrap(), authored);
            assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
        }
    }

    #[test]
    fn ai21_existing_inherited_graphic_and_nonrectangular_containers_remain_fixed() {
        for kind in ["existing", "inherited", "graphic", "ellipse"] {
            let (mut session, mut plan, _) = contained_growth_fixture();
            if kind == "existing" || kind == "inherited" {
                let mut background = plan.commands.remove(0).params;
                if kind == "inherited" {
                    background["spread"] = json!({"kind":"parent","index":0});
                }
                session.execute("frame.create", &background).unwrap();
                plan.commands[2].params["ids"] = json!(["$0.id"]);
            } else if kind == "graphic" {
                plan.commands[0].params["content"] = json!("graphic");
            } else {
                plan.commands[0].params["shape"] = json!("ellipse");
            }
            let authored = serde_json::to_value(&plan).unwrap();
            let base = session.doc().unwrap().clone();
            assert!(prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).is_err(), "{kind} must not be resized by local recovery");
            assert_eq!(serde_json::to_value(&plan).unwrap(), authored);
            assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
        }
    }

    fn fitted_footer_fixture() -> (Session, ProductionPlan, Rect) {
        let (session, mut plan, _) = contained_growth_fixture();
        plan.commands.remove(0);
        plan.commands.pop();
        plan.commands[2].params["ids"] = json!(["$0.id"]);
        let safe = session.doc().unwrap().doc.spreads[0].pages[0].margin_rect();
        let old_bottom = plan.commands[0].params["rect"][3].as_f64().unwrap();
        let dy = safe.y1 - old_bottom;
        plan.commands[0].params["rect"][1] = json!(60.0 + dy);
        plan.commands[0].params["rect"][3] = json!(safe.y1);
        (session, plan, Rect::new(60.0, 60.0 + dy, 225.0, safe.y1))
    }

    #[test]
    fn ai21_tiny_overset_footer_moves_up_with_unchanged_copy_fonts_and_explicit_insets() {
        let (session, mut plan, original) = fitted_footer_fixture();
        let authored = plan.clone();
        let base = session.doc().unwrap().clone();
        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(notes.iter().any(|note| note["kind"] == "nativeTextTranslation"));
        assert_eq!(plan.commands[0].params["text"], authored.commands[0].params["text"]);
        assert_eq!(plan.commands[1].params, authored.commands[1].params);
        assert_eq!(plan.commands[2].params, authored.commands[2].params);
        let mut proof = Session::new();
        proof.prefs = session.prefs.clone();
        proof.add_document(base.clone());
        let result =
            proof.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
        let doc = &proof.doc().unwrap().doc;
        let bounds = doc.item(ItemId(result["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
        assert!(contains(doc.spreads[0].pages[0].margin_rect(), bounds));
        assert!((bounds.y1 - original.y1).abs() < 0.02);
        assert!(original.y0 - bounds.y0 > 4.0 && original.y0 - bounds.y0 < 5.0);
        assert_eq!((bounds.x0, bounds.x1), (original.x0, original.x1));
        assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
    }

    #[test]
    fn ai21_footer_growth_refuses_upward_obstruction_and_reports_bounded_source_identity() {
        let (session, mut plan, original) = fitted_footer_fixture();
        plan.commands.push(EngineAction {
            command: "frame.create".into(),
            params: json!({"spread":0,"content":"graphic","rect":[60,original.y0-3.0,225,original.y0-1.0]}),
        });
        let authored = serde_json::to_value(&plan).unwrap();
        let base = session.doc().unwrap().clone();
        let error = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap_err();
        assert!(error.contains("source $0") && error.contains("item ") && error.contains("rect ["), "{error}");
        assert!(error.contains("Count the flowers") && error.contains("4.4 pt"), "{error}");
        assert!(error.len() < 700, "Overset diagnostics must remain bounded");
        assert_eq!(serde_json::to_value(&plan).unwrap(), authored);
        assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
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

    fn stacked_header_fixture(card: bool, overlap: f64) -> (Session, ProductionPlan, usize, usize) {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false,"margins":42.52})).unwrap();
        let mut plan: ProductionPlan = serde_json::from_value(json!({"title":"Stacked unit header","commands":[],"images":[]})).unwrap();
        if card {
            plan.commands
                .push(EngineAction { command: "frame.create".into(), params: json!({"spread":0,"rect":[290,44,548,112],"content":"unassigned"}) });
        }
        let upper = plan.commands.len();
        for (rect, text) in [([300.0, 50.0, 540.0, 72.0 + overlap], "Unit Two"), ([300.0, 72.0, 540.0, 98.0], "Numbers and Patterns")] {
            let index = plan.commands.len();
            plan.commands.push(EngineAction {
                command: "frame.create".into(),
                params: json!({"spread":0,"rect":rect,"content":"text","text":text,"caret":false}),
            });
            plan.commands
                .push(EngineAction { command: "type.char".into(), params: json!({"attrs":{"size":10,"leading":{"kind":"points","value":12}}}) });
            plan.commands
                .push(EngineAction { command: "object.textFrameOptions".into(), params: json!({"ids":[format!("${index}.id")],"inset":[1,2,1,2]}) });
        }
        (session, plan, upper, upper + 3)
    }

    #[test]
    fn ai22_stacked_header_translation_keeps_copy_fonts_size_and_containing_card() {
        for (card, overlap) in [(true, 2.0), (false, 2.0), (true, 12.0)] {
            let (session, mut plan, upper, lower) = stacked_header_fixture(card, overlap);
            let authored = plan.clone();
            let base = session.doc().unwrap().clone();
            let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
            assert_eq!(plan.commands.len(), authored.commands.len() + 1, "Only the lower header may move");
            for (before, after) in authored.commands.iter().zip(&plan.commands) {
                assert_eq!(before.command, after.command);
                assert_eq!(before.params, after.params, "Authored copy, font size, insets, rectangles and source indices must remain unchanged");
            }
            let movement = plan.commands.last().unwrap();
            assert_eq!(movement.command, "transform.move");
            assert_eq!(movement.params["ids"], json!([format!("${lower}.id")]));
            assert_eq!(movement.params["dx"].as_f64().unwrap(), 0.0);
            assert!((movement.params["dy"].as_f64().unwrap() - overlap).abs() < 0.02, "Use the minimal downward translation");
            assert_eq!(notes.iter().filter(|note| note["kind"] == "nativeTextTranslation").count(), 1);
            let mut proof = Session::new();
            proof.prefs = session.prefs.clone();
            proof.add_document(base.clone());
            let applied =
                proof.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":true})).unwrap();
            let doc = &proof.doc().unwrap().doc;
            let upper_bounds = doc.item(ItemId(applied["results"][upper]["id"].as_u64().unwrap())).unwrap().bounds();
            let lower_bounds = doc.item(ItemId(applied["results"][lower]["id"].as_u64().unwrap())).unwrap().bounds();
            assert_eq!(upper_bounds, Rect::new(300.0, 50.0, 540.0, 72.0 + overlap));
            assert!(!overlaps(upper_bounds, lower_bounds));
            assert_eq!((lower_bounds.x0, lower_bounds.x1, lower_bounds.height()), (300.0, 540.0, 26.0));
            assert!((lower_bounds.y0 - 72.0 - overlap).abs() < 0.02);
            assert!(contains(doc.spreads[0].pages[0].margin_rect(), lower_bounds));
            if card {
                let card_bounds = doc.item(ItemId(applied["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
                assert_eq!(card_bounds, Rect::new(290.0, 44.0, 548.0, 112.0));
                assert!(contains(card_bounds, upper_bounds) && contains(card_bounds, lower_bounds));
            }
            assert_eq!(doc.story(StoryId(applied["results"][upper]["story"].as_u64().unwrap())).unwrap().text, "Unit Two");
            assert_eq!(doc.story(StoryId(applied["results"][lower]["story"].as_u64().unwrap())).unwrap().text, "Numbers and Patterns");
            assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
        }
    }

    #[test]
    fn ai22_stacked_header_translation_refuses_real_inherited_image_and_new_body_obstructions() {
        for kind in ["existing", "inherited", "image", "new_body", "new_artwork", "card_edge"] {
            let (mut session, mut plan, _, _) = stacked_header_fixture(true, 2.0);
            let obstacle = json!({"spread":if kind == "inherited" {json!({"kind":"parent","index":0})} else {json!(0)},"rect":[300,99,540,125],"content":"unassigned"});
            match kind {
                "existing" | "inherited" => {
                    session.execute("frame.create", &obstacle).unwrap();
                }
                "image" => plan.images.push(designcraft_ai_types::protocol::ImagePlacement {
                    repeat_rects: Vec::new(),
                    repeat_groups: Vec::new(),
                    prompt: "Synthetic obstacle".into(),
                    page: 0,
                    rect: [300.0, 99.0, 540.0, 125.0],
                    label: "Fixed illustration".into(),
                }),
                "new_body" => {
                    plan.commands.push(EngineAction {
                        command: "frame.create".into(),
                        params: json!({"spread":0,"rect":[300,99,540,125],"content":"text","text":"Practice below","caret":false}),
                    });
                    plan.commands.push(EngineAction {
                        command: "type.char".into(),
                        params: json!({"attrs":{"size":10,"leading":{"kind":"points","value":12}}}),
                    });
                }
                "new_artwork" => plan.commands.push(EngineAction { command: "frame.create".into(), params: obstacle }),
                "card_edge" => plan.commands[0].params["rect"][3] = json!(98.5),
                _ => panic!("Unexpected obstruction fixture: {kind}"),
            }
            let authored = serde_json::to_value(&plan).unwrap();
            let base = session.doc().unwrap().clone();
            assert!(prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).is_err(), "{kind} must block header recovery");
            assert_eq!(serde_json::to_value(&plan).unwrap(), authored, "{kind} must leave the authored plan intact");
            assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
        }
    }

    #[test]
    fn ai22_stacked_header_translation_rejects_mixed_duplicate_ambiguous_large_and_body_crossings() {
        for kind in ["mixed", "identical", "partial_column", "large_overlap", "body"] {
            let (session, mut plan, upper, lower) = stacked_header_fixture(false, 2.0);
            match kind {
                "mixed" => plan.commands[lower].params["text"] = json!("Numbers ب"),
                "identical" => plan.commands[lower].params["rect"] = plan.commands[upper].params["rect"].clone(),
                "partial_column" => plan.commands[lower].params["rect"][0] = json!(310),
                "large_overlap" => plan.commands[upper].params["rect"][3] = json!(84.1),
                "body" => {
                    plan.commands[upper].params["rect"] = json!([300, 122, 540, 146]);
                    plan.commands[lower].params["rect"] = json!([300, 144, 540, 170]);
                }
                _ => panic!("Unexpected header fixture: {kind}"),
            }
            let authored = serde_json::to_value(&plan).unwrap();
            let base = session.doc().unwrap().clone();
            assert!(prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).is_err(), "{kind} must remain an explicit layout error");
            assert_eq!(serde_json::to_value(&plan).unwrap(), authored);
            assert!(std::sync::Arc::ptr_eq(&base.doc, &session.doc().unwrap().doc));
        }
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

    #[test]
    fn unregistered_inline_cmyk_and_bare_swatch_names_resolve_to_existing_swatches() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Inline swatch recovery",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 200, 120], "content": "unassigned"}},
                {"command": "object.fill", "params": {"ids": ["$0.id"], "swatch": "C=0 M=15 Y=95 K=0", "tint": 0.15}},
                {"command": "object.stroke", "params": {"ids": ["$0.id"], "swatch": "Paper", "weight": 1.0}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 140, 200, 200], "content": "unassigned"}},
                {"command": "object.fill", "params": {"ids": ["$3.id"], "swatch": "C:85 M:75 Y:0 K:0", "tint": 1.0}},
                {"command": "object.stroke", "params": {"ids": ["$3.id"], "swatch": "Transparent", "weight": 0.0}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 220, 200, 280], "content": "unassigned"}},
                {"command": "object.fill", "params": {"ids": ["$6.id"], "swatch": "Sunshine Yellow", "tint": 0.2}},
                {"command": "object.stroke", "params": {"ids": ["$6.id"], "swatch": "#FF0", "weight": 1.0}}
            ]
        }))
        .unwrap();
        let base = session.doc().unwrap().clone();
        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert_eq!(plan.commands[1].params["swatch"], "C=0 M=0 Y=100 K=0");
        assert_eq!(plan.commands[2].params["swatch"], "[Paper]");
        assert_eq!(plan.commands[4].params["swatch"], "C=100 M=90 Y=10 K=0");
        assert_eq!(plan.commands[5].params["swatch"], "[None]");
        assert_eq!(plan.commands[7].params["swatch"], "C=0 M=0 Y=100 K=0");
        assert_eq!(plan.commands[8].params["swatch"], "C=0 M=0 Y=100 K=0");
        assert_eq!(notes.iter().filter(|n| n["kind"] == "nativeSwatchFallback").count(), 6);
    }

    #[test]
    fn command_aliases_including_type_set_normalize_to_vetted_engine_commands() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Command alias normalization",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 300, 120], "content": "text", "text": "Initial", "caret": false}},
                {"command": "type.set", "params": {"id": "$0.story", "content": "Updated Copy"}},
                {"command": "style.para.create", "params": {"name": "TitleStyle", "chars": {"size": 16}}},
                {"command": "style.para.apply", "params": {"name": "TitleStyle"}},
                {"command": "lists.bulleted", "params": {}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 150, 140, 230], "shape": "ellipse", "content": "unassigned"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [100, 150, 180, 230], "shape": "ellipse", "content": "unassigned"}},
                {"command": "pathfinder.add", "params": {"ids": ["$5.id", "$6.id"]}},
                {"command": "object.cornerRadius", "params": {"ids": ["$7.id"], "radius": 8}},
                {"command": "object.move", "params": {"ids": ["$7.id"], "dx": 4, "dy": 4}},
                {"command": "object.gradientFill", "params": {"ids": ["$7.id"], "kind": "linear", "stops": [{"pos": 0.0, "color": "#1E3A8A"}, {"pos": 1.0, "color": "#3B82F6"}]}},
                {"command": "qr.create", "params": {"type": "url", "content": "https://example.com", "rect": [360, 150, 440, 230]}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 260, 520, 460], "content": "text", "text": "", "caret": true}},
                {"command": "table.insert", "params": {"rows": 2, "cols": 2, "width": 440}},
                {"command": "table.insertCol", "params": {"story": "$12.story", "table": "$13.id", "col": 0, "where": "right", "count": 1}},
                {"command": "table.rowHeight", "params": {"story": "$12.story", "table": "$13.id", "row": 0, "height": 26}},
                {"command": "table.columnWidth", "params": {"story": "$12.story", "table": "$13.id", "col": 0, "width": 120}},
                {"command": "table.setHeaderFooterRows", "params": {"story": "$12.story", "table": "$13.id", "headerRows": 1, "repeatHeader": "everyTextColumn", "altRows": {"everyA": 1, "colorA": "Paper", "everyB": 1, "colorB": "Paper"}}}
            ]
        }))
        .unwrap();
        let base = session.doc().unwrap().clone();
        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert_eq!(plan.commands[1].command, "story.setText");
        assert_eq!(plan.commands[1].params["story"], "$0.story");
        assert_eq!(plan.commands[1].params["text"], "Updated Copy");
        assert_eq!(plan.commands[2].command, "style.paragraph.create");
        assert_eq!(plan.commands[3].command, "style.paragraph.apply");
        assert_eq!(plan.commands[4].command, "type.para");
        assert_eq!(plan.commands[4].params["attrs"]["listType"], "bullets");
        assert_eq!(plan.commands[7].command, "object.pathfinder");
        assert_eq!(plan.commands[7].params["op"], "add");
        assert_eq!(plan.commands[8].command, "object.cornerOptions");
        assert_eq!(plan.commands[8].params["size"], 8);
        assert_eq!(plan.commands[8].params["shape"], "rounded");
        assert_eq!(plan.commands[9].command, "transform.move");
        assert_eq!(plan.commands[10].command, "object.gradient");
        assert_eq!(plan.commands[10].params["stops"][0]["location"], 0.0);
        assert_eq!(plan.commands[10].params["stops"][1]["location"], 100.0);
        assert_eq!(plan.commands[11].command, "object.qrCode");
        assert_eq!(plan.commands[14].command, "table.insertColumn");
        assert_eq!(plan.commands[14].params["table"], "$13.table");
        assert_eq!(plan.commands[15].command, "table.setRowHeight");
        assert_eq!(plan.commands[16].command, "table.setColumnWidth");
        assert_eq!(plan.commands[17].command, "table.options");
        assert_eq!(plan.commands[17].params["repeatHeader"], true);
        assert_eq!(plan.commands[17].params["altRows"]["first"], 1);
        assert_eq!(plan.commands[17].params["altRows"]["firstColor"], "[Paper]");
        assert!(notes.iter().any(|n| n["kind"] == "commandAliasNormalized"));
    }

    #[test]
    fn implicit_frame_text_content_and_post_create_height_overrides_recover_without_overset() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
        let copy = "Line one of a multi-line callout box that requires more than twenty-four points of vertical height to compose cleanly without any overset.";
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Implicit text and post-create transform",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 220, 84], "text": copy, "caret": false}},
                {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": copy.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 18}}}},
                {"command": "transform.set", "params": {"ids": ["$0.id"], "height": 24}}
            ]
        }))
        .unwrap();
        let base = session.doc().unwrap().clone();
        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert_eq!(plan.commands[0].params["content"], "text");
        assert!(notes.iter().any(|n| n["kind"] == "nativeTextReflow"));
        session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
    }

    #[test]
    fn overset_frame_blocked_below_shifts_upward_into_verified_free_space() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
        let copy = "This caption sits directly above an illustration divider and needs two extra lines, while generous free space remains above it.";
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Upward mid-page fit",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 220, 220, 244], "content": "text", "text": copy, "caret": false}},
                {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": copy.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 18}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 250, 220, 360], "content": "graphic"}}
            ]
        }))
        .unwrap();
        let base = session.doc().unwrap().clone();
        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(notes.iter().any(|n| n["kind"] == "nativeTextTranslation"));
        session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
    }

    #[test]
    fn opened_document_story_edit_recovers_newly_induced_overset_into_free_space() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
        let created =
            session.execute("frame.create", &json!({"spread":0,"rect":[60,60,220,86],"content":"text","text":"Short","caret":false})).unwrap();
        let story_id = created["story"].as_u64().unwrap();
        let base = session.doc().unwrap().clone();
        let expanded_copy =
            "Expanded editorial paragraph replacing a short title in an opened document where ample vertical room exists below the frame.";
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Opened document story edit",
            "images": [],
            "commands": [
                {"command": "story.setText", "params": {"story": story_id, "text": expanded_copy}},
                {"command": "text.select", "params": {"story": story_id, "anchor": 0, "focus": expanded_copy.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 18}}}}
            ]
        }))
        .unwrap();
        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(notes.iter().any(|n| n["kind"] == "nativeTextReflow"));
        session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
    }

    #[test]
    fn overset_frame_followed_by_alignment_or_scaling_avoids_double_geometry_mutation() {
        let copy = "This footer paragraph requires three lines at 13 pt so the initial 22 pt box overflows before bottom-margin alignment runs.";
        for mode in ["align_bottom", "scale_center"] {
            let mut session = Session::new();
            session.execute("file.new", &json!({"preset":"A4","pages":1,"facingPages":false})).unwrap();
            let base = session.doc().unwrap().clone();
            let post_action = if mode == "align_bottom" {
                json!({"command": "object.align", "params": {"ids": "$0.id", "align": "bottom", "to": "margins"}})
            } else {
                json!({"command": "transform.scale", "params": {"ids": ["$0.id"], "sx": 1.05, "sy": 1.05}})
            };
            let mut plan: ProductionPlan = serde_json::from_value(json!({
                "title": "Post-create geometry parity",
                "images": [],
                "commands": [
                    {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 200.0, 320.0, 222.0], "content": "text", "text": copy, "caret": false}},
                    {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": copy.len()}},
                    {"command": "type.char", "params": {"attrs": {"size": 13, "leading": {"kind": "points", "value": 17}}}},
                    post_action
                ]
            }))
            .unwrap();
            let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
            assert!(notes.iter().any(|n| n["kind"] == "nativeTextReflow"), "{mode}");
            assert_eq!(
                plan.commands[0].params["rect"][3],
                json!(222.0),
                "{mode}: original creation height must stay intact before post-create geometry"
            );
            session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
        }
    }

    #[test]
    fn honorific_and_symbol_glyph_fallbacks_preserve_bold_and_preflight_cleanly_in_new_and_opened_stories() {
        let db = designcraft_fonts::FontDb::global();
        let Some(primary) = ["Tahoma", "Cairo", "DejaVu Sans", "Arial"]
            .into_iter()
            .find(|f| db.has_family(f) && ["Regular", "Bold"].iter().all(|s| db.styles(f).iter().any(|st| st == s)))
        else {
            return;
        };
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        let existing = session
            .execute("frame.create", &json!({"spread": 0, "rect": [60, 320, 520, 400], "content": "text", "text": "Initial", "caret": false}))
            .unwrap();
        let existing_story = existing["story"].as_u64().unwrap();
        let base = session.doc().unwrap().clone();
        let scoped = db.scoped(base.doc.font_scope);
        for ch in ['\u{fdfa}', '\u{fdfd}', '★', '✓', '⭐'] {
            if !scoped.face(primary, "Regular").covers(ch) && resolve_fallback_face(&scoped, ch, false, false).is_none() {
                return;
            }
        }
        let new_copy = "صلى الله عليه وسلم ﷺ وبسم الله ﷽ ★ ✓ ⭐";
        let bold_end = new_copy.find(" وبسم").unwrap();
        let edited_copy = "مراجعة الدرس ✓ ونجمة التفوق ⭐ ﷺ";
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Honorific and symbol glyph fallback",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 520, 140], "content": "text", "text": new_copy, "caret": false}},
                {"command": "type.para", "params": {"attrs": {"direction": "rightToLeft", "align": "right"}}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": primary, "fontStyle": "Regular", "size": 15, "leading": {"kind": "points", "value": 22}}}},
                {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": bold_end}},
                {"command": "type.char", "params": {"attrs": {"fontStyle": "Bold"}}},
                {"command": "story.setText", "params": {"story": existing_story, "text": edited_copy}},
                {"command": "text.select", "params": {"story": existing_story, "anchor": 0, "focus": edited_copy.len()}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": primary, "fontStyle": "Bold", "size": 15, "leading": {"kind": "points", "value": 22}}}}
            ]
        }))
        .unwrap();
        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        if !scoped.face(primary, "Regular").covers('\u{fdfa}') || !scoped.face(primary, "Regular").covers('⭐') {
            assert!(notes.iter().any(|n| n["kind"] == "symbolFontFallback"), "notes={notes:?}");
        }
        session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
    }

    #[test]
    fn container_enclosed_header_and_wrapped_short_badge_recover_without_overset_or_collision() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        let base = session.doc().unwrap().clone();

        // Case A: Subtitle [54, 83, 280, 96] (13 pt tall) inside a non-growable graphic header banner [44, 44, 550, 98]
        // needing ~4 pt more height shifts upward into the >= 8 pt top clearance inside the banner.
        let subtitle = "Unit Three: Patterns";
        let mut header_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Container-enclosed header upward shift",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [44, 44, 550, 96.5], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [54, 83, 280, 96], "content": "text", "text": subtitle, "caret": false}},
                {"command": "text.select", "params": {"story": "$1.story", "anchor": 0, "focus": subtitle.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 19}}}}
            ]
        }))
        .unwrap();
        let header_notes = prepare(&mut header_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(header_notes.iter().any(|n| n["kind"] == "nativeTextTranslation"), "header_notes={header_notes:?}");
        let mut proof_a = Session::new();
        proof_a.add_document(base.clone());
        proof_a
            .execute("production.apply", &json!({"commands": header_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();

        // Case B: Short single-line badge label wrapped into 2 lines inside a vertical sandwich (blocked above and below)
        // recovers via bounded horizontal widening into free safe space, while horizontal obstruction fails atomically.
        let badge_copy = "Rectangle • Shape";
        let mut badge_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Wrapped short badge horizontal widening",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [200, 200, 380, 239], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [240, 241, 322, 261], "content": "text", "text": badge_copy, "caret": false}},
                {"command": "text.select", "params": {"story": "$1.story", "anchor": 0, "focus": badge_copy.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 12, "leading": {"kind": "points", "value": 15}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [200, 263, 380, 310], "content": "graphic"}}
            ]
        }))
        .unwrap();
        let badge_notes = prepare(&mut badge_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(badge_notes.iter().any(|n| n["kind"] == "nativeTextReflow" && n.get("extraWidthPt").is_some()), "badge_notes={badge_notes:?}");
        let mut proof_b = Session::new();
        proof_b.add_document(base.clone());
        proof_b
            .execute("production.apply", &json!({"commands": badge_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();

        // Blocking both left and right horizontal space must reject atomically without mutating the plan.
        let mut blocked_badge: ProductionPlan = serde_json::from_value(json!({
            "title": "Blocked badge",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [200, 200, 380, 239], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [240, 241, 322, 261], "content": "text", "text": badge_copy, "caret": false}},
                {"command": "text.select", "params": {"story": "$1.story", "anchor": 0, "focus": badge_copy.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 12, "leading": {"kind": "points", "value": 15}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [200, 263, 380, 310], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [180, 241, 238, 261], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [324, 241, 390, 261], "content": "graphic"}}
            ]
        }))
        .unwrap();
        let blocked_before = serde_json::to_value(&blocked_badge).unwrap();
        assert!(prepare(&mut blocked_badge, &base, &session.prefs, &AtomicBool::new(false)).is_err());
        assert_eq!(serde_json::to_value(&blocked_badge).unwrap(), blocked_before);
    }

    #[test]
    fn null_formatting_and_selection_step_references_normalize_to_target_producers() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        let base = session.doc().unwrap().clone();
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Null formatting step references",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 300, 110], "content": "text", "text": "Header box", "caret": false}},
                {"command": "type.para", "params": {"attrs": {"align": "center"}}},
                {"command": "object.stroke", "params": {"ids": ["$1.id"], "swatch": "[Black]", "weight": 1.0}},
                {"command": "selection.set", "params": {"ids": ["$0.id"]}},
                {"command": "text.select", "params": {"story": "$3.story", "anchor": 0, "focus": 6}},
                {"command": "type.char", "params": {"attrs": {"size": 14}}}
            ]
        }))
        .unwrap();
        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert_eq!(plan.commands[2].params["ids"], json!(["$0.id"]));
        assert_eq!(plan.commands[4].params["story"], json!("$0.story"));
        assert!(notes.iter().any(|n| n["kind"] == "commandAliasNormalized"));
        session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();

        // Off-by-one forward non-text creation after an implicit formatting step must resolve to the next non-text item.
        let mut forward_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Off-by-one forward non-text",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 300, 110], "content": "text", "text": "Title", "caret": false}},
                {"command": "type.para", "params": {"attrs": {"align": "center"}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 130, 180, 210], "content": "unassigned"}},
                {"command": "object.stroke", "params": {"ids": ["$1.id"], "swatch": "[Black]", "weight": 1.5}}
            ]
        }))
        .unwrap();
        normalize_command_aliases(&mut forward_plan);
        assert_eq!(forward_plan.commands[3].params["ids"], json!(["$2.id"]));
    }

    #[test]
    fn rtl_facing_spread_step_and_repeat_group_pathfinder_and_gradient_apply_cleanly() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 3, "facingPages": true})).unwrap();
        session.execute("layout.documentSetup", &json!({"binding": "rightToLeft"})).unwrap();
        let rtl_base = session.doc().unwrap().clone();
        assert!(rtl_base.doc.settings.facing_pages && rtl_base.doc.settings.right_to_left_binding);
        let canonical = crate::facing_pages::canonicalize(&rtl_base, &session.prefs).unwrap();

        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "RTL spread vector composition",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 1, "rect": [72.0, 90.0, 140.0, 150.0], "content": "unassigned"}},
                {"command": "edit.stepAndRepeat", "params": {"ids": ["$0.id"], "count": 2, "dx": 90.0, "dy": 0.0}},
                {"command": "object.group", "params": {"ids": "$1.ids"}},
                {"command": "object.gradient", "params": {
                    "ids": "$1.ids",
                    "kind": "linear",
                    "from": [162.0, 90.0],
                    "to": [320.0, 150.0],
                    "stops": [{"color": "#0F172A", "location": 0.0}, {"color": "#38BDF8", "location": 100.0}]
                }},
                {"command": "frame.create", "params": {"spread": 2, "rect": [90.0, 120.0, 180.0, 210.0], "shape": "ellipse", "content": "unassigned"}},
                {"command": "frame.create", "params": {"spread": 2, "rect": [140.0, 120.0, 230.0, 210.0], "shape": "ellipse", "content": "unassigned"}},
                {"command": "object.pathfinder", "params": {"op": "add", "ids": ["$4.id", "$5.id"]}},
                {"command": "object.gradient", "params": {
                    "ids": ["$6.id"],
                    "kind": "linear",
                    "from": [90.0, 120.0],
                    "to": [230.0, 210.0],
                    "stops": [{"color": "#1E3A8A", "location": 0.0}, {"color": "#F59E0B", "location": 100.0}]
                }}
            ]
        }))
        .unwrap();

        prepare(&mut plan, &canonical, &session.prefs, &AtomicBool::new(false)).unwrap();
        let mapped = crate::facing_pages::map_plan(&plan, &rtl_base).unwrap();
        session.execute("production.apply", &json!({"commands": mapped.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
        crate::facing_pages::validate_mapped_result(&rtl_base, session.doc().unwrap(), crate::worker::BatchScope { start: 1, count: 2 }).unwrap();
    }

    #[test]
    fn opened_document_simultaneous_frame_resize_and_story_edit_recovers_cleanly() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        let created = session
            .execute("frame.create", &json!({"spread": 0, "rect": [60, 60, 200, 90], "content": "text", "text": "Old title", "caret": false}))
            .unwrap();
        let frame_id = created["id"].as_u64().unwrap();
        let story_id = created["story"].as_u64().unwrap();
        let base = session.doc().unwrap().clone();

        let updated_copy =
            "Expanded callout paragraph in an opened document whose width and copy are edited together, requiring additional vertical height below.";
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Simultaneous frame resize and story edit",
            "images": [],
            "commands": [
                {"command": "transform.set", "params": {"ids": [frame_id], "width": 220.0, "height": 34.0, "ref": 0}},
                {"command": "story.setText", "params": {"story": story_id, "text": updated_copy}},
                {"command": "text.select", "params": {"story": story_id, "anchor": 0, "focus": updated_copy.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 18}}}}
            ]
        }))
        .unwrap();

        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(notes.iter().any(|n| n["kind"] == "nativeTextReflow"));
        session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
        let after = session.doc().unwrap();
        assert!(!session.cache.get(&after.doc, StoryId(story_id), None).is_overset());
        assert!((after.doc.item(ItemId(frame_id)).unwrap().bounds().width() - 220.0).abs() < 0.05);
    }

    #[test]
    fn run_069_subtitle_widen_and_partial_banner_upward_shift_recover_without_overset() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        let base = session.doc().unwrap().clone();

        // Exact Run [069] geometry: 40-char, 7-word subtitle [54, 76, 260, 96] inside banner [46, 44, 548, 98],
        // blocked above by title [54, 48, 280, 75] and with 54 pt free horizontal space before [314, 76, 540, 96].
        let subtitle = "Numbers 15 & 16: Ocean Treasure Counting";
        let mut run_069_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Run 069 subtitle horizontal widening",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [46, 44, 548, 98], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [54, 48, 280, 75], "content": "text", "text": "UNIT FOUR", "caret": false}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [54, 76, 260, 96], "content": "text", "text": subtitle, "caret": false}},
                {"command": "text.select", "params": {"story": "$2.story", "anchor": 0, "focus": subtitle.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 12.5, "leading": {"kind": "points", "value": 16.0}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [314, 76, 540, 96], "content": "graphic"}}
            ]
        }))
        .unwrap();
        let widen_notes = prepare(&mut run_069_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(widen_notes.iter().any(|n| n["kind"] == "nativeTextReflow" && n.get("extraWidthPt").is_some()), "notes={widen_notes:?}");
        let mut proof_069 = Session::new();
        proof_069.add_document(base.clone());
        proof_069
            .execute("production.apply", &json!({"commands": run_069_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();

        // Partial upward shift inside a fixed banner: downward growth (~93.5 pt) exceeds fixed graphic banner [46, 44, 548, 92],
        // full upward shift (-5.5 pt -> y0=54.5) is blocked by upper accent rule at y1=56.5, while partial shift (-2.76 pt -> y0=57.24, y1=90.76) fits cleanly.
        let banner_heading = "Lesson 12: Exploring Coral Reefs\nCounting & Comparing";
        let mut partial_banner_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Partial banner upward shift",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [46, 44, 548, 92], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [54, 48, 220, 56.5], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [54, 60, 340, 88], "content": "text", "text": banner_heading, "caret": false}},
                {"command": "text.select", "params": {"story": "$2.story", "anchor": 0, "focus": banner_heading.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 14, "leading": {"kind": "points", "value": 19}}}}
            ]
        }))
        .unwrap();
        let shift_notes = prepare(&mut partial_banner_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(shift_notes.iter().any(|n| n["kind"] == "nativeTextTranslation"), "shift_notes={shift_notes:?}");
        let mut proof_banner = Session::new();
        proof_banner.add_document(base);
        proof_banner
            .execute("production.apply", &json!({"commands": partial_banner_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();
    }

    #[test]
    fn enclosed_numerals_ornate_parens_and_variation_selectors_survive_print_fonts_and_fallback() {
        let db = designcraft_fonts::FontDb::global();
        let Some(primary) = ["Tahoma", "Cairo", "DejaVu Sans", "Arial"]
            .into_iter()
            .find(|f| db.has_family(f) && ["Regular", "Bold"].iter().all(|s| db.styles(f).iter().any(|st| st == s)))
        else {
            return;
        };
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        let base = session.doc().unwrap().clone();
        let scoped = db.scoped(base.doc.font_scope);
        let copy = "① ⅓ ﴾بِسْمِ اللَّهِ﴿ ﷺ ﷽ ⭐\u{FE0F} ✓\u{FE0F}";
        for ch in ['①', '⅓', '﴾', '﴿', '\u{FDFA}', '\u{FDFD}', '⭐', '✓'] {
            if !scoped.face(primary, "Regular").covers(ch) && !has_fallback_glyph(&scoped, ch) {
                return;
            }
        }
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Extended symbol & honorific fallback with print fonts",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 520, 140], "content": "text", "text": copy, "caret": false}},
                {"command": "type.para", "params": {"attrs": {"direction": "rightToLeft", "align": "right"}}},
                {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": copy.len()}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": primary, "fontStyle": "Regular", "size": 15, "leading": {"kind": "points", "value": 22}}}}
            ]
        }))
        .unwrap();
        let contract =
            crate::print_contract::PrintContract { english_fonts: vec![primary.into()], arabic_fonts: vec![primary.into()], ..Default::default() };
        if contract.select_fonts(&base).is_ok() {
            crate::native_print_fonts::apply(&mut plan, &base, &session.prefs, &contract).unwrap();
        }
        prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        session.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
    }

    #[test]
    fn run_006_non_story_references_extended_null_steps_text_thread_and_edit_clear_normalize_cleanly() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        let base = session.doc().unwrap().clone();

        // Case 1: Paired container + inner text frame where text.select references container $0.story (Run [006] pattern A).
        let mut paired_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Paired container story ref",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 320, 130], "content": "unassigned"}},
                {"command": "object.fill", "params": {"ids": ["$0.id"], "swatch": "[Paper]"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [68, 68, 312, 122], "content": "text", "text": "Card heading", "caret": false}},
                {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": 12}},
                {"command": "type.char", "params": {"attrs": {"size": 14}}}
            ]
        }))
        .unwrap();
        prepare(&mut paired_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert_eq!(paired_plan.commands[3].params["story"], json!("$2.story"));
        let mut s1 = Session::new();
        s1.add_document(base.clone());
        s1.execute("production.apply", &json!({"commands": paired_plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();

        // Case 2: frame.create omitting content="text" followed by story.setText & text.select targeting $0.story (Run [006] pattern B).
        let mut omitted_content_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Omitted content=text frame",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 150, 320, 220]}},
                {"command": "story.setText", "params": {"story": "$0.story", "text": "Populated story"}},
                {"command": "text.select", "params": {"story": "$0.story", "anchor": 0, "focus": 9}},
                {"command": "type.char", "params": {"attrs": {"size": 13}}}
            ]
        }))
        .unwrap();
        prepare(&mut omitted_content_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert_eq!(omitted_content_plan.commands[0].params["content"], json!("text"));
        let mut s2 = Session::new();
        s2.add_document(base.clone());
        s2.execute("production.apply", &json!({"commands": omitted_content_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();

        // Case 3: Extended null steps (transform.rotate, object.dropShadow) in text.thread from/to + edit.clear invalidation.
        let mut thread_and_clear_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Extended null steps in text.thread and edit.clear invalidation",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 60, 240, 120], "content": "text", "text": "Column A", "caret": false}},
                {"command": "object.dropShadow", "params": {"ids": ["$0.id"], "on": true, "distance": 2.0, "size": 4.0}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [260, 60, 440, 120], "content": "text", "text": "", "caret": false}},
                {"command": "transform.rotate", "params": {"ids": ["$2.id"], "angle": 0.0}},
                {"command": "text.thread", "params": {"from": "$1.id", "to": "$3.id"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [60, 200, 140, 250], "content": "unassigned"}},
                {"command": "object.fill", "params": {"ids": ["$5.id"], "swatch": "[Black]"}},
                {"command": "edit.clear", "params": {"ids": ["$5.id"]}},
                {"command": "object.stroke", "params": {"ids": ["$6.id"], "swatch": "[Black]", "weight": 1.0}}
            ]
        }))
        .unwrap();
        normalize_command_aliases(&mut thread_and_clear_plan);
        assert_eq!(thread_and_clear_plan.commands[4].params["from"], json!("$0.id"));
        assert_eq!(thread_and_clear_plan.commands[4].params["to"], json!("$2.id"));
        assert_eq!(thread_and_clear_plan.commands[8].params["ids"], json!(["$6.id"]), "deleted $5.id must not stay active after edit.clear");
    }

    #[test]
    fn tight_container_insets_and_run_098_snug_pill_badges_recover_without_overset() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        let base = session.doc().unwrap().clone();

        // 1. Tight 4 pt top-inset card header [52, 102, 286, 115] inside [46, 98, 292, 246]
        // with obstacle at y0=115.6 requiring combined partial upward + downward gap fit.
        let card_header = "Identify 2D & 3D Shapes";
        let mut tight_top_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Tight 4pt top-inset card header",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [46, 98, 292, 246], "content": "unassigned"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [52, 102, 286, 115], "content": "text", "text": card_header, "caret": false}},
                {"command": "text.select", "params": {"story": "$1.story", "anchor": 0, "focus": card_header.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 14.0, "leading": {"kind": "points", "value": 19.0}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [52, 115.6, 286, 220], "content": "graphic"}}
            ]
        }))
        .unwrap();
        let notes_top = prepare(&mut tight_top_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(notes_top.iter().any(|n| n["kind"] == "nativeTextTranslation"), "notes_top={notes_top:?}");
        let mut s_top = Session::new();
        s_top.add_document(base.clone());
        s_top
            .execute("production.apply", &json!({"commands": tight_top_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();

        // 2. Tight 4 pt left-inset frame [58, 558, 136, 578] inside [54, 554, 210, 756], blocked vertically, widens rightward.
        let side_copy = "Count & Circle Shapes";
        let mut tight_side_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Tight 4pt side-inset widening",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [54, 554, 210, 756], "content": "unassigned"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [58, 558, 136, 578], "content": "text", "text": side_copy, "caret": false}},
                {"command": "text.select", "params": {"story": "$1.story", "anchor": 0, "focus": side_copy.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 12.0, "leading": {"kind": "points", "value": 15.0}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [58, 579, 206, 700], "content": "graphic"}}
            ]
        }))
        .unwrap();
        let notes_side = prepare(&mut tight_side_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(notes_side.iter().any(|n| n["kind"] == "nativeTextReflow" && n.get("extraWidthPt").is_some()), "notes_side={notes_side:?}");
        let mut s_side = Session::new();
        s_side.add_document(base.clone());
        s_side
            .execute("production.apply", &json!({"commands": tight_side_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();

        // 3. Exact Run [098] badge [429, 243, 535, 261] ("مستطيل • Rectangle") enclosed in snug pill [425, 241, 539, 263]
        // inside outer card [408, 98, 548, 360], blocked below at y0=264.0 with free upward space.
        let db = designcraft_fonts::FontDb::global();
        let ar_font = ["Cairo", "Tahoma", "Arial", "DejaVu Sans"].into_iter().find(|f| db.has_family(f)).unwrap_or("DejaVu Sans");
        let run_098_copy = "مستطيل • Rectangle";
        let mut run_098_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Run 098 snug pill badge upward recovery",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [408, 98, 548, 360], "content": "unassigned"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [425, 241, 539, 263], "content": "unassigned"}},
                {"command": "object.cornerOptions", "params": {"ids": ["$1.id"], "shape": "rounded", "size": 8.0}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [429, 243, 535, 261], "content": "text", "text": run_098_copy, "caret": false}},
                {"command": "text.select", "params": {"story": "$3.story", "anchor": 0, "focus": run_098_copy.len()}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": ar_font, "size": 12.5, "leading": {"kind": "points", "value": 16.5}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [416, 264, 540, 348], "content": "graphic"}}
            ]
        }))
        .unwrap();
        prepare(&mut run_098_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        let mut s_098 = Session::new();
        s_098.add_document(base.clone());
        let applied_098 = s_098
            .execute("production.apply", &json!({"commands": run_098_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();
        let doc_098 = &s_098.doc().unwrap().doc;
        let card_b = doc_098.item(ItemId(applied_098["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
        let pill_b = doc_098.item(ItemId(applied_098["results"][1]["id"].as_u64().unwrap())).unwrap().bounds();
        let text_b = doc_098.item(ItemId(applied_098["results"][3]["id"].as_u64().unwrap())).unwrap().bounds();
        assert!(contains(card_b, pill_b) && contains(pill_b, text_b), "card={card_b:?} pill={pill_b:?} text={text_b:?}");

        // 4. Snug 44x14 pill badge [275, 726, 319, 740] ("Cylinder") inside snug pill [271, 724, 323, 742],
        // blocked vertically above & below, widens horizontally together with its pill background.
        let cyl = "Cylinder";
        let mut snug_pill_widen: ProductionPlan = serde_json::from_value(json!({
            "title": "Snug 44x14 pill horizontal widening",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [250, 680, 344, 723], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [271, 724, 323, 742], "content": "unassigned"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [275, 726, 319, 740], "content": "text", "text": cyl, "caret": false}},
                {"command": "text.select", "params": {"story": "$2.story", "anchor": 0, "focus": cyl.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 11.5, "leading": {"kind": "points", "value": 13.0}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [250, 743, 344, 770], "content": "graphic"}}
            ]
        }))
        .unwrap();
        prepare(&mut snug_pill_widen, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        let mut s_cyl = Session::new();
        s_cyl.add_document(base);
        let applied_cyl = s_cyl
            .execute("production.apply", &json!({"commands": snug_pill_widen.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();
        let doc_cyl = &s_cyl.doc().unwrap().doc;
        let cyl_pill = doc_cyl.item(ItemId(applied_cyl["results"][1]["id"].as_u64().unwrap())).unwrap().bounds();
        let cyl_text = doc_cyl.item(ItemId(applied_cyl["results"][2]["id"].as_u64().unwrap())).unwrap().bounds();
        assert!(contains(cyl_pill, cyl_text), "cyl_pill={cyl_pill:?} cyl_text={cyl_text:?}");
    }

    #[test]
    fn omitted_content_stroked_containers_pills_and_multiline_tight_inset_cards_recover_cleanly() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"preset": "A4", "pages": 1, "facingPages": false})).unwrap();
        let base = session.doc().unwrap().clone();
        let db = designcraft_fonts::FontDb::global();
        let ar_font = ["Cairo", "Tahoma", "Arial", "DejaVu Sans"].into_iter().find(|f| db.has_family(f)).unwrap_or("DejaVu Sans");

        // 1. Real LLM plans omit `"content"` on card/pill `frame.create` actions and apply strokes.
        // Run [098] badge [429, 243, 535, 261] inside omitted-content stroked pill [425, 241, 539, 263]
        // and omitted-content stroked card [408, 98, 548, 360].
        let run_098_copy = "مستطيل • Rectangle";
        let mut omitted_pill_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Omitted-content stroked Run 098 pill",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [408.0, 98.0, 548.0, 360.0]}},
                {"command": "object.stroke", "params": {"ids": ["$0.id"], "swatch": "[Black]", "weight": 1.0}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [425.0, 241.0, 539.0, 263.0]}},
                {"command": "object.stroke", "params": {"ids": ["$2.id"], "swatch": "[Black]", "weight": 1.5}},
                {"command": "object.cornerOptions", "params": {"ids": ["$2.id"], "shape": "rounded", "size": 8.0}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [429.0, 243.0, 535.0, 261.0], "content": "text", "text": run_098_copy, "caret": false}},
                {"command": "text.select", "params": {"story": "$5.story", "anchor": 0, "focus": run_098_copy.len()}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": ar_font, "size": 12.5, "leading": {"kind": "points", "value": 16.5}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [416.0, 265.0, 540.0, 348.0], "content": "graphic"}}
            ]
        }))
        .unwrap();
        prepare(&mut omitted_pill_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        let mut s1 = Session::new();
        s1.add_document(base.clone());
        let r1 = s1
            .execute("production.apply", &json!({"commands": omitted_pill_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();
        let d1 = &s1.doc().unwrap().doc;
        let card_v = d1.item(ItemId(r1["results"][0]["id"].as_u64().unwrap())).unwrap().visible_bounds();
        let pill_v = d1.item(ItemId(r1["results"][2]["id"].as_u64().unwrap())).unwrap().visible_bounds();
        let text_v = d1.item(ItemId(r1["results"][5]["id"].as_u64().unwrap())).unwrap().visible_bounds();
        let gfx_v = d1.item(ItemId(r1["results"][8]["id"].as_u64().unwrap())).unwrap().visible_bounds();
        assert!(contains(card_v, pill_v) && contains(pill_v, text_v) && !overlaps(pill_v, gfx_v), "pill={pill_v:?} text={text_v:?} gfx={gfx_v:?}");

        // 2. Omitted-content 44x14 pill widening horizontally together with its enclosed short label.
        let cyl = "Cylinder";
        let mut omitted_widen_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Omitted-content snug pill horizontal widening",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [250.0, 680.0, 344.0, 722.0], "content": "graphic"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [271.0, 724.0, 323.0, 742.0]}},
                {"command": "object.stroke", "params": {"ids": ["$1.id"], "swatch": "[Black]", "weight": 1.0}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [275.0, 726.0, 319.0, 740.0], "content": "text", "text": cyl, "caret": false}},
                {"command": "text.select", "params": {"story": "$3.story", "anchor": 0, "focus": cyl.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 11.5, "leading": {"kind": "points", "value": 13.0}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [250.0, 744.0, 344.0, 770.0], "content": "graphic"}}
            ]
        }))
        .unwrap();
        prepare(&mut omitted_widen_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        let mut s2 = Session::new();
        s2.add_document(base.clone());
        let r2 = s2
            .execute("production.apply", &json!({"commands": omitted_widen_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();
        let d2 = &s2.doc().unwrap().doc;
        let p2 = d2.item(ItemId(r2["results"][1]["id"].as_u64().unwrap())).unwrap().bounds();
        let t2 = d2.item(ItemId(r2["results"][3]["id"].as_u64().unwrap())).unwrap().bounds();
        assert!(contains(p2, t2), "p2={p2:?} t2={t2:?}");

        // 3. Multi-line (42 pt tall, > 36 pt) prompt frame [314, 256, 540, 298] with 4 pt top inset
        // inside omitted-content card [46, 252, 548, 486] blocked below by graphic [54, 299.5, 540, 480] (last-rejected-response.txt $62/$69/$73).
        let multiline_prompt = "Trace the dashed symmetry line carefully, complete the missing half of each shape, and color both sides evenly.";
        let mut card_multiline_plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Multi-line 42pt frame inside 4pt-inset authored card",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [46.0, 252.0, 548.0, 486.0]}},
                {"command": "object.stroke", "params": {"ids": ["$0.id"], "swatch": "[Black]", "weight": 1.5}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [314.0, 256.0, 540.0, 298.0], "content": "text", "text": multiline_prompt, "caret": false}},
                {"command": "text.select", "params": {"story": "$2.story", "anchor": 0, "focus": multiline_prompt.len()}},
                {"command": "type.char", "params": {"attrs": {"size": 11.0, "leading": {"kind": "points", "value": 15.5}}}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [54.0, 298.1, 540.0, 480.0], "content": "graphic"}}
            ]
        }))
        .unwrap();
        let notes3 = prepare(&mut card_multiline_plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(notes3.iter().any(|n| n["kind"] == "nativeTextTranslation"), "notes3={notes3:?}");
        let mut s3 = Session::new();
        s3.add_document(base);
        let r3 = s3
            .execute("production.apply", &json!({"commands": card_multiline_plan.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();
        let d3 = &s3.doc().unwrap().doc;
        let card3 = d3.item(ItemId(r3["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
        let text3 = d3.item(ItemId(r3["results"][2]["id"].as_u64().unwrap())).unwrap().bounds();
        let gfx3 = d3.item(ItemId(r3["results"][5]["id"].as_u64().unwrap())).unwrap().bounds();
        assert!(contains(card3, text3) && text3.y0 >= card3.y0 + 1.25 && !overlaps(text3, gfx3), "card3={card3:?} text3={text3:?} gfx3={gfx3:?}");

        // 4. Single-text container omitting `"content"` grows downward when clear, and refuses padding-band obstacles without crushing top padding.
        let (s_cfg, mut omitted_wrap, bg_wrap) = contained_growth_fixture();
        omitted_wrap.commands[0].params.as_object_mut().unwrap().remove("content");
        let mut blocked_wrap = omitted_wrap.clone();
        blocked_wrap.commands.push(EngineAction {
            command: "frame.create".into(),
            params: json!({"spread": 0, "content": "graphic", "rect": [54.0, bg_wrap.y1 + 1.0, 59.0, bg_wrap.y1 + 3.0]}),
        });
        let notes4 = prepare(&mut omitted_wrap, s_cfg.doc().unwrap(), &s_cfg.prefs, &AtomicBool::new(false)).unwrap();
        assert!(notes4.iter().any(|n| n["kind"] == "nativeBackgroundReflow"));
        assert!(prepare(&mut blocked_wrap, s_cfg.doc().unwrap(), &s_cfg.prefs, &AtomicBool::new(false)).is_err());
    }

    #[test]
    fn ai28_grouped_rounded_pill_expands_on_overset_and_combining_mark_fallback_keeps_cluster() {
        let copy = "نَشَاطٌ تَعْلِيمِيٌّ مُشَكَّلٌ: اِخْتَرِ الْحَرْفَ الصَّحِيحَ وَضَعْ دَائِرَةً حَوْلَ الإِجَابَةِ";
        let all_chars: Vec<char> = copy.chars().collect();
        let base_only_chars: Vec<char> = copy.chars().filter(|&ch| !is_combining_arabic_mark(ch)).collect();
        let fonts = designcraft_fonts::FontDb::global();
        fonts.add_font(designcraft_fonts::testing::font_with("DC AI28 Pill Base", &base_only_chars).unwrap());
        fonts.add_font(designcraft_fonts::testing::font_with("DC AI28 Pill Full", &all_chars).unwrap());

        let mut session = Session::new();
        session.execute("file.new", &json!({"width": 595, "height": 842, "pages": 1, "facingPages": false, "margins": [36, 36, 36, 36]})).unwrap();
        let base = session.doc().unwrap().clone();

        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Grouped rounded pill overset expansion",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [72.0, 100.0, 320.0, 130.0]}},
                {"command": "object.cornerOptions", "params": {"ids": ["$0.id"], "shape": "rounded", "size": 12.0}},
                {"command": "object.fill", "params": {"ids": ["$0.id"], "swatch": "[Paper]"}},
                {"command": "frame.create", "params": {"spread": 0, "rect": [82.0, 104.0, 310.0, 126.0], "content": "text", "text": copy, "caret": false}},
                {"command": "text.select", "params": {"story": "$3.story", "anchor": 0, "focus": copy.len()}},
                {"command": "type.char", "params": {"attrs": {"fontFamily": "DC AI28 Pill Base", "size": 12.0, "leading": {"kind": "points", "value": 18.0}}}},
                {"command": "object.group", "params": {"ids": ["$0.id", "$3.id"]}}
            ]
        }))
        .unwrap();

        let notes = prepare(&mut plan, &base, &session.prefs, &AtomicBool::new(false)).unwrap();
        assert!(notes.iter().any(|n| n["kind"] == "symbolFontFallback"), "expected combining-mark cluster fallback: {notes:?}");
        assert!(
            plan.commands.iter().skip(7).any(|cmd| cmd.command == "text.select" && cmd.params["anchor"] == 0),
            "first combining-mark fallback must include leading base letter byte 0: {:?}",
            &plan.commands[7..]
        );
        assert!(notes.iter().any(|n| n["kind"] == "nativeBackgroundReflow"), "expected grouped rounded pill to expand: {notes:?}");
        assert!(notes.iter().any(|n| n["kind"] == "nativeTextReflow"), "expected grouped text frame to expand: {notes:?}");

        let mut verify = Session::new();
        verify.add_document(base);
        let applied =
            verify.execute("production.apply", &json!({"commands": plan.commands, "outputMode": "native_only", "requirePreflight": true})).unwrap();
        let doc = &verify.doc().unwrap().doc;
        let pill = doc.item(ItemId(applied["results"][0]["id"].as_u64().unwrap())).unwrap().bounds();
        let text = doc.item(ItemId(applied["results"][3]["id"].as_u64().unwrap())).unwrap().bounds();
        assert!(pill.height() > 30.0 && text.height() > 22.0 && contains(pill, text), "pill={pill:?} text={text:?}");
    }

    #[test]
    fn ai28_parse_inline_color_and_swatch_recovery_do_not_panic_on_multibyte_hash_strings() {
        assert!(parse_inline_color("#aأ").is_none());
        assert!(parse_inline_color("#aأbcd").is_none());
        assert!(parse_inline_color("#أب").is_none());
        assert!(parse_inline_color("#1a2b3c").is_some());
        assert!(parse_inline_color("#abc").is_some());

        let mut session = Session::new();
        session.execute("file.new", &json!({"pages": 1, "facingPages": false})).unwrap();
        let base = session.doc().unwrap().clone();
        let mut plan: ProductionPlan = serde_json::from_value(json!({
            "title": "Multibyte swatch safety",
            "images": [],
            "commands": [
                {"command": "frame.create", "params": {"spread": 0, "rect": [36.0, 36.0, 120.0, 120.0]}},
                {"command": "object.fill", "params": {"ids": ["$0.id"], "swatch": "#aأ"}},
                {"command": "object.stroke", "params": {"ids": ["$0.id"], "color": "#aأbcd"}}
            ]
        }))
        .unwrap();
        let notes = recover_unregistered_swatches(&mut plan, &base).unwrap();
        assert!(notes.is_empty());
        assert_eq!(plan.commands[1].params["swatch"], "#aأ");
        assert_eq!(plan.commands[2].params["color"], "#aأbcd");
    }
}
