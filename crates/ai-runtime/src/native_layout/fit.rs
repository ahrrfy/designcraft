use super::{
    AtomicBool, BTreeMap, BTreeSet, BackgroundGrowth, DocState, EngineAction, FittingObstacle, ItemId, Ordering, Prefs, ProductionPlan, Rect,
    Session, StoryId, Value, apply_story_glyph_fallbacks, containing_background_growths, contains, fitting_obstacles, format_populated_new_stories,
    is_allowed_fit_location, is_expandable_card_shape, is_snug_growable_background, json, normalize_command_aliases, normalize_unit_interval_fields,
    overlaps, position_collision, recover_unregistered_swatches, shift,
};

struct CreatedFrameSets {
    frames: Vec<(usize, ItemId, StoryId)>,
    frame_refs: BTreeMap<ItemId, (usize, String, String)>,
    created_frames: BTreeMap<ItemId, usize>,
}

struct WidenedLabelSolution {
    session: Session,
    dw: f64,
    dx: f64,
    dh_extra: f64,
    dy_extra: f64,
    pill_ops: Vec<(usize, f64, f64, f64, f64)>,
}

struct FrameFitTarget {
    fit_index: usize,
    fit_id: ItemId,
    fit_is_existing_base: bool,
    fit_item_ref: Value,
    spread_index: usize,
    page: usize,
    old: Rect,
    grown: Rect,
    safe: Rect,
    mark_pad: f64,
    extra_height: f64,
    orig_w: f64,
    orig_h: f64,
    label: String,
    story_id: StoryId,
    path_len: usize,
}

struct ContainerEnvironment {
    snug_pills: BTreeMap<ItemId, (usize, Rect)>,
    enclosing_containers: Vec<(Rect, Option<f64>)>,
    compact_frame: bool,
}

impl ContainerEnvironment {
    fn collect(
        target: &FrameFitTarget,
        doc: &designcraft_engine::doc::Document,
        base: &DocState,
        created_frames: &BTreeMap<ItemId, usize>,
        candidate: &ProductionPlan,
        cancel: &AtomicBool,
    ) -> Result<(Self, Vec<FittingObstacle>), String> {
        let all_obstacles = fitting_obstacles(doc, target.spread_index, cancel)?;
        let snug_pills: BTreeMap<ItemId, (usize, Rect)> = all_obstacles
            .iter()
            .filter_map(|obs| {
                let idx = is_snug_growable_background(
                    obs,
                    target.fit_id,
                    target.old,
                    target.page,
                    doc,
                    base,
                    created_frames,
                    &all_obstacles,
                    candidate,
                    target.spread_index,
                )?;
                Some((obs.id, (idx, obs.bounds)))
            })
            .collect();
        let mut enclosing_containers: Vec<(Rect, Option<f64>)> = all_obstacles
            .iter()
            .filter(|obs| (obs.inherited || obs.id != target.fit_id) && !snug_pills.contains_key(&obs.id) && contains(obs.bounds, target.old))
            .map(|obs| {
                let panel_min_y0 = if !obs.inherited
                    && created_frames.contains_key(&obs.id)
                    && base.doc.item(obs.id).is_none()
                    && obs.bounds.y1 >= target.grown.y1 + 16.0
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
                .filter(|img| img.page == target.spread_index)
                .flat_map(|img| img.all_rects())
                .map(|[x0, y0, x1, y1]| Rect::new(x0, y0, x1, y1))
                .filter(|&bounds| contains(bounds, target.old))
                .map(|bounds| (bounds, None)),
        );
        let compact_frame = target.old.height() <= 36.0;
        Ok((Self { snug_pills, enclosing_containers, compact_frame }, all_obstacles))
    }

    fn container_ok(&self, old: Rect, candidate_rect: Rect) -> bool {
        self.enclosing_containers.iter().all(|&(c, panel_min_y0)| {
            if self.compact_frame {
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
    }

    fn horiz_container_ok(&self, old: Rect, r: Rect) -> bool {
        self.enclosing_containers.iter().all(|&(c, _)| {
            let left_pad = (old.x0 - c.x0).clamp(0.0, 2.0);
            let right_pad = (c.x1 - old.x1).clamp(0.0, 2.0);
            r.x0 >= c.x0 + left_pad && r.x1 <= c.x1 - right_pad && contains(c, r)
        })
    }
}

fn collect_created_and_text_frames(
    candidate: &ProductionPlan,
    results: &[Value],
    current_doc: &designcraft_engine::doc::Document,
) -> Result<CreatedFrameSets, String> {
    let mut frames = Vec::new();
    let mut frame_refs: BTreeMap<ItemId, (usize, String, String)> = BTreeMap::new();
    let mut created_frames = BTreeMap::new();
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
    Ok(CreatedFrameSets { frames, frame_refs, created_frames })
}

fn build_upward_shift_candidates(
    target: &FrameFitTarget,
    env: &ContainerEnvironment,
    all_obstacles: &[FittingObstacle],
    candidate: &ProductionPlan,
) -> (Vec<f64>, f64) {
    let full_up_dy = target.old.y1 - target.grown.y1;
    let pill_bot_pad = env.snug_pills.values().map(|(_, r)| (r.y1 - target.old.y1).max(0.0)).fold(0.0_f64, f64::max);
    let min_allowed_dy = full_up_dy - pill_bot_pad - 0.5;
    let mut shift_candidates: Vec<f64> = [1.0_f64, 0.85, 0.75, 0.65, 0.5, 0.35, 0.25, 0.15].into_iter().map(|frac| full_up_dy * frac).collect();
    let safe_ceil_dy = target.safe.y0 - target.grown.y0;
    if safe_ceil_dy < 0.0 && safe_ceil_dy > full_up_dy {
        shift_candidates.push(safe_ceil_dy);
    }
    if !env.enclosing_containers.is_empty() {
        let min_c_y1 = env.enclosing_containers.iter().map(|&(c, _)| c.y1 - (c.y1 - target.old.y1).clamp(0.0, 0.5)).fold(f64::INFINITY, f64::min);
        if min_c_y1.is_finite() {
            shift_candidates.push(min_c_y1 - target.grown.y1);
        }
        for &(c, panel_min_y0) in &env.enclosing_containers {
            if env.compact_frame {
                let top_pad = (target.old.y0 - c.y0).clamp(0.0, 0.5);
                let c_ceil_dy = (c.y0 + top_pad) - target.grown.y0;
                if c_ceil_dy < 0.0 && c_ceil_dy > full_up_dy {
                    shift_candidates.push(c_ceil_dy);
                }
            } else if let Some(min_y0) = panel_min_y0 {
                let c_ceil_dy = min_y0 - target.grown.y0;
                if c_ceil_dy < 0.0 && c_ceil_dy > full_up_dy {
                    shift_candidates.push(c_ceil_dy);
                }
            }
        }
    }
    let x_min = env.snug_pills.values().map(|(_, r)| r.x0).fold(target.old.x0, f64::min);
    let x_max = env.snug_pills.values().map(|(_, r)| r.x1).fold(target.old.x1, f64::max);
    for obs_bounds in all_obstacles
        .iter()
        .filter(|obs| (obs.inherited || (obs.id != target.fit_id && !env.snug_pills.contains_key(&obs.id))) && !contains(obs.bounds, target.old))
        .map(|obs| obs.bounds)
        .chain(
            candidate
                .images
                .iter()
                .filter(|img| img.page == target.spread_index)
                .flat_map(|img| img.all_rects())
                .map(|[x0, y0, x1, y1]| Rect::new(x0, y0, x1, y1))
                .filter(|&b| !contains(b, target.old)),
        )
    {
        if obs_bounds.x0 < x_max && obs_bounds.x1 > x_min && obs_bounds.y0 >= target.old.y1 - 0.05 && obs_bounds.y0 <= target.grown.y1 + 16.0 {
            shift_candidates.push((obs_bounds.y0 - 0.02) - target.grown.y1);
            if pill_bot_pad > 0.0 {
                shift_candidates.push((obs_bounds.y0 - pill_bot_pad - 0.02) - target.grown.y1);
            }
        }
    }
    (shift_candidates, min_allowed_dy)
}

fn select_upward_shift_growth(
    target: &FrameFitTarget,
    env: &ContainerEnvironment,
    shift_info: (&[f64], f64),
    doc: &designcraft_engine::doc::Document,
    base: &DocState,
    created_frames: &BTreeMap<ItemId, usize>,
    state_cancel: (&ProductionPlan, &AtomicBool),
) -> Result<Option<(TextPosition, Vec<BackgroundGrowth>)>, String> {
    let (shift_candidates, min_allowed_dy) = shift_info;
    let (candidate, cancel) = state_cancel;
    let max_container_extra =
        if env.compact_frame || env.enclosing_containers.iter().all(|&(_, panel_min_y0)| panel_min_y0.is_some()) { 16.0 } else { 12.0 };
    for &dy_candidate in shift_candidates {
        if !dy_candidate.is_finite() || dy_candidate > 0.0 || dy_candidate < min_allowed_dy {
            continue;
        }
        let shifted = shift(target.grown, 0.0, dy_candidate);
        let container_safe =
            env.enclosing_containers.is_empty() || (target.extra_height <= max_container_extra && env.container_ok(target.old, shifted));
        if !container_safe || !contains(target.safe, shifted) {
            continue;
        }
        let upward_growth = TextPosition {
            index: target.fit_index,
            id: target.fit_id,
            spread: target.spread_index,
            page: target.page,
            original: target.old,
            target: shifted,
            safe: target.safe,
            language: 0,
        };
        let Some(up_bgs) = containing_background_growths(&upward_growth, doc, base, created_frames, candidate, cancel)? else {
            continue;
        };
        if !up_bgs.iter().all(|bg| {
            env.enclosing_containers.iter().all(|&(c, _)| bg.target.y0 >= c.y0 + 0.5 && bg.target.y1 <= c.y1 - 0.5 && contains(c, bg.target))
        }) {
            continue;
        }
        let mut targets = BTreeMap::from([(target.fit_id, shifted)]);
        for bg in &up_bgs {
            targets.insert(bg.id, bg.target);
        }
        if !position_collision(&upward_growth, &targets, doc, candidate, cancel)? {
            return Ok(Some((upward_growth, up_bgs)));
        }
    }
    Ok(None)
}

fn build_pill_horiz_targets(
    target: &FrameFitTarget,
    env: &ContainerEnvironment,
    widened: Rect,
    dw: f64,
    dx: f64,
) -> Option<Vec<(ItemId, usize, Rect, Rect, bool)>> {
    let mut pill_horiz_targets = Vec::with_capacity(env.snug_pills.len());
    for (&pill_id, &(pill_idx, pill_bounds)) in &env.snug_pills {
        let (pill_target, needs_widen) = if pill_bounds.x0 <= widened.x0 - 0.5 && pill_bounds.x1 >= widened.x1 + 0.5 {
            (pill_bounds, false)
        } else {
            (Rect::new(pill_bounds.x0 + dx, pill_bounds.y0, pill_bounds.x1 + dx + dw, pill_bounds.y1), true)
        };
        if !contains(target.safe, pill_target)
            || !contains(pill_target, widened)
            || !env
                .enclosing_containers
                .iter()
                .all(|&(c, _)| pill_target.x0 >= c.x0 + 0.5 && pill_target.x1 <= c.x1 - 0.5 && contains(c, pill_target))
        {
            return None;
        }
        pill_horiz_targets.push((pill_id, pill_idx, pill_bounds, pill_target, needs_widen));
    }
    Some(pill_horiz_targets)
}

fn try_combined_widen_vertical(
    target: &FrameFitTarget,
    env: &ContainerEnvironment,
    horiz_geom: (Rect, f64, f64, f64, &[(ItemId, usize, Rect, Rect, bool)]),
    pre_fit_state: &DocState,
    prefs: &Prefs,
    doc: &designcraft_engine::doc::Document,
    state_cancel: (&ProductionPlan, &AtomicBool),
) -> Result<Option<WidenedLabelSolution>, String> {
    let (widened, dw, dx, rem_dh, pill_horiz_targets) = horiz_geom;
    let (candidate, cancel) = state_cancel;
    for dy_w in [0.0_f64, -rem_dh, -0.5 * rem_dh] {
        let comb_rect = Rect::new(widened.x0, widened.y0 + dy_w, widened.x1, widened.y1 + rem_dh + dy_w);
        if !contains(target.safe, comb_rect) || !env.container_ok(target.old, comb_rect) || !env.horiz_container_ok(target.old, comb_rect) {
            continue;
        }
        let mut comb_targets = BTreeMap::from([(target.fit_id, comb_rect)]);
        let mut comb_pills = Vec::with_capacity(pill_horiz_targets.len());
        let mut comb_pills_ok = true;
        for &(pill_id, pill_idx, pill_orig, pill_h_rect, needs_widen) in pill_horiz_targets {
            let pill_full = if contains(pill_h_rect, comb_rect) {
                pill_h_rect
            } else {
                Rect::new(pill_h_rect.x0, pill_h_rect.y0 + dy_w.min(0.0), pill_h_rect.x1, pill_h_rect.y1 + (rem_dh + dy_w).max(0.0))
            };
            if !contains(target.safe, pill_full)
                || !contains(pill_full, comb_rect)
                || !env.enclosing_containers.iter().all(|&(c, _)| {
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
            index: target.fit_index,
            id: target.fit_id,
            spread: target.spread_index,
            page: target.page,
            original: target.old,
            target: comb_rect,
            safe: target.safe,
            language: 0,
        };
        if position_collision(&comb_pos, &comb_targets, doc, candidate, cancel)? {
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
                spread: target.spread_index,
                page: target.page,
                original: pill_orig,
                target: pill_full,
                safe: target.safe,
                language: 0,
            };
            if position_collision(&pp, &comb_targets, doc, candidate, cancel)? {
                comb_pill_collision = true;
                break;
            }
        }
        if comb_pill_collision {
            continue;
        }
        if let Some(sol) = execute_combined_widen_session(target, (dw, dx, rem_dh, dy_w), &comb_pills, pre_fit_state, prefs)? {
            return Ok(Some(sol));
        }
    }
    Ok(None)
}

fn execute_combined_widen_session(
    target: &FrameFitTarget,
    deltas: (f64, f64, f64, f64),
    comb_pills: &[(ItemId, usize, Rect, Rect, bool)],
    pre_fit_state: &DocState,
    prefs: &Prefs,
) -> Result<Option<WidenedLabelSolution>, String> {
    let (dw, dx, rem_dh, dy_w) = deltas;
    let mut comb_session = Session::new();
    comb_session.prefs = prefs.clone();
    comb_session.add_document(pre_fit_state.clone());
    let mut comb_pill_ops = Vec::new();
    for &(pill_id, pill_idx, pill_orig, pill_full, needs_widen) in comb_pills {
        let Some(pill_item) = pre_fit_state.doc.item(pill_id) else { continue };
        let base_pw = if prefs.dimensions_include_stroke { pill_item.visible_bounds().width() } else { pill_item.bounds().width() };
        let base_ph = if prefs.dimensions_include_stroke { pill_item.visible_bounds().height() } else { pill_item.bounds().height() };
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
            comb_session.execute("transform.move", &json!({"ids":[pill_id.0],"dx":p_dx,"dy":p_dy})).map_err(|e| e.to_string())?;
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
        .execute("transform.set", &json!({"ids":[target.fit_id.0],"width":target.orig_w + dw,"height":target.orig_h + rem_dh + 0.01,"ref":0}))
        .map_err(|e| e.to_string())?;
    if dx.abs() > 0.001 || dy_w.abs() > 0.001 {
        comb_session.execute("transform.move", &json!({"ids":[target.fit_id.0],"dx":dx,"dy":dy_w})).map_err(|e| e.to_string())?;
    }
    let comb_doc = comb_session.doc().map_err(|e| e.to_string())?.doc.clone();
    if !comb_session.cache.get(&comb_doc, target.story_id, None).is_overset() {
        Ok(Some(WidenedLabelSolution { session: comb_session, dw, dx, dh_extra: rem_dh + 0.01, dy_extra: dy_w, pill_ops: comb_pill_ops }))
    } else {
        Ok(None)
    }
}

fn search_widened_label_solution(
    target: &FrameFitTarget,
    env: &ContainerEnvironment,
    pre_fit_state: &DocState,
    prefs: &Prefs,
    doc: &designcraft_engine::doc::Document,
    state_cancel: (&ProductionPlan, &AtomicBool),
) -> Result<Option<WidenedLabelSolution>, String> {
    let (candidate, cancel) = state_cancel;
    for dw in [4.0_f64, 8.0, 12.0, 18.0, 24.0, 30.0, 36.0, 48.0, 64.0] {
        for dx_factor in [-0.5_f64, 0.0, -1.0, -0.25, -0.75, 0.5] {
            let dx = dw * dx_factor;
            let widened = Rect::new(target.old.x0 + dx, target.old.y0, target.old.x1 + dx + dw, target.old.y1);
            if !contains(target.safe, widened) || !env.horiz_container_ok(target.old, widened) {
                continue;
            }
            let Some(pill_horiz_targets) = build_pill_horiz_targets(target, env, widened, dw, dx) else {
                continue;
            };
            let mut wide_targets = BTreeMap::from([(target.fit_id, widened)]);
            for &(pill_id, _, _, pill_target, _) in &pill_horiz_targets {
                wide_targets.insert(pill_id, pill_target);
            }
            let wide_pos = TextPosition {
                index: target.fit_index,
                id: target.fit_id,
                spread: target.spread_index,
                page: target.page,
                original: target.old,
                target: widened,
                safe: target.safe,
                language: 0,
            };
            if position_collision(&wide_pos, &wide_targets, doc, candidate, cancel)? {
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
                    spread: target.spread_index,
                    page: target.page,
                    original: pill_orig,
                    target: pill_target,
                    safe: target.safe,
                    language: 0,
                };
                if position_collision(&pill_pos, &wide_targets, doc, candidate, cancel)? {
                    pill_collides = true;
                    break;
                }
            }
            if pill_collides {
                continue;
            }
            if let Some(sol) = try_widen_step(target, env, (widened, dw, dx, &pill_horiz_targets), pre_fit_state, prefs, doc, state_cancel)? {
                return Ok(Some(sol));
            }
        }
    }
    Ok(None)
}

fn try_widen_step(
    target: &FrameFitTarget,
    env: &ContainerEnvironment,
    horiz_step: (Rect, f64, f64, &[(ItemId, usize, Rect, Rect, bool)]),
    pre_fit_state: &DocState,
    prefs: &Prefs,
    doc: &designcraft_engine::doc::Document,
    state_cancel: (&ProductionPlan, &AtomicBool),
) -> Result<Option<WidenedLabelSolution>, String> {
    let (widened, dw, dx, pill_horiz_targets) = horiz_step;
    let mut wide_session = Session::new();
    wide_session.prefs = prefs.clone();
    wide_session.add_document(pre_fit_state.clone());
    let mut pill_ops = Vec::new();
    for &(pill_id, pill_idx, _, _, needs_widen) in pill_horiz_targets {
        if !needs_widen {
            continue;
        }
        let Some(pill_item) = pre_fit_state.doc.item(pill_id) else { continue };
        let pill_w = if prefs.dimensions_include_stroke { pill_item.visible_bounds().width() } else { pill_item.bounds().width() };
        wide_session.execute("transform.set", &json!({"ids":[pill_id.0],"width":pill_w + dw,"ref":0})).map_err(|e| e.to_string())?;
        if dx.abs() > 0.001 {
            wide_session.execute("transform.move", &json!({"ids":[pill_id.0],"dx":dx,"dy":0.0})).map_err(|e| e.to_string())?;
        }
        pill_ops.push((pill_idx, pill_w + dw, 0.0_f64, dx, 0.0_f64));
    }
    wide_session.execute("transform.set", &json!({"ids":[target.fit_id.0],"width":target.orig_w + dw,"ref":0})).map_err(|e| e.to_string())?;
    if dx.abs() > 0.001 {
        wide_session.execute("transform.move", &json!({"ids":[target.fit_id.0],"dx":dx,"dy":0.0})).map_err(|e| e.to_string())?;
    }
    let wide_doc = wide_session.doc().map_err(|e| e.to_string())?.doc.clone();
    if !wide_session.cache.get(&wide_doc, target.story_id, None).is_overset() {
        return Ok(Some(WidenedLabelSolution { session: wide_session, dw, dx, dh_extra: 0.0, dy_extra: 0.0, pill_ops }));
    }
    // If horizontal widening unwrapped the label to a single line whose glyph box
    // still needs a small vertical adjustment (<= 10 pt), verify combined recovery.
    let mut measure_wide = Session::new();
    measure_wide.prefs = prefs.clone();
    let wide_state = wide_session.doc().map_err(|e| e.to_string())?.clone();
    measure_wide.add_document(wide_state);
    if measure_wide.execute("object.fit", &json!({"ids":[target.fit_id.0],"mode":"fitFrameToContent","axis":"height"})).is_ok()
        && let Ok(ms_doc) = measure_wide.doc()
        && let Some(ms_item) = ms_doc.doc.item(target.fit_id)
    {
        let rem_dh = (ms_item.bounds().height() - widened.height()).max(0.0);
        if rem_dh > 0.0 && rem_dh <= 10.0 && !measure_wide.cache.get(&ms_doc.doc, target.story_id, None).is_overset() {
            return try_combined_widen_vertical(target, env, (widened, dw, dx, rem_dh, pill_horiz_targets), pre_fit_state, prefs, doc, state_cancel);
        }
    }
    Ok(None)
}

fn apply_widened_label_solution(
    target: &FrameFitTarget,
    sol: WidenedLabelSolution,
    scratch: &mut Session,
    candidate: &mut ProductionPlan,
    notes: &mut Vec<Value>,
) -> Result<(), String> {
    let WidenedLabelSolution { session: wide_session, dw, dx, dh_extra, dy_extra, pill_ops } = sol;
    let pill_cmd_count: usize =
        pill_ops.iter().map(|&(_, w, h, p_dx, p_dy)| usize::from(w > 0.0 || h > 0.0) + usize::from(p_dx.abs() > 0.001 || p_dy.abs() > 0.001)).sum();
    let needed_cmds = pill_cmd_count.saturating_add(1).saturating_add(usize::from(dx.abs() > 0.001 || dy_extra.abs() > 0.001));
    if candidate.commands.len().checked_add(needed_cmds).is_none_or(|len| len > 512) {
        return Err("Local fitting exceeds the bounded native action limit".into());
    }
    *scratch = wide_session;
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
            candidate.commands.push(EngineAction { command: "transform.move".into(), params: json!({"ids":[pill_ref],"dx":p_dx,"dy":p_dy}) });
        }
    }
    let mut fit_set = serde_json::Map::new();
    fit_set.insert("ids".into(), json!([target.fit_item_ref.clone()]));
    fit_set.insert("width".into(), json!(target.orig_w + dw));
    if dh_extra > 0.0 {
        fit_set.insert("height".into(), json!(target.orig_h + dh_extra));
    }
    fit_set.insert("ref".into(), json!(0));
    candidate.commands.push(EngineAction { command: "transform.set".into(), params: Value::Object(fit_set) });
    if dx.abs() > 0.001 || dy_extra.abs() > 0.001 {
        candidate
            .commands
            .push(EngineAction { command: "transform.move".into(), params: json!({"ids":[target.fit_item_ref.clone()],"dx":dx,"dy":dy_extra}) });
    }
    notes.push(json!({
        "kind": "nativeTextReflow",
        "page": target.spread_index,
        "item": target.fit_id.0,
        "label": target.label,
        "extraWidthPt": dw,
        "extraHeightPt": dh_extra,
        "dx": dx,
        "dy": dy_extra,
        "message": "Widened wrapped short label into verified safe horizontal space; preserved copy and font size"
    }));
    Ok(())
}

fn apply_vertical_growth_commands(
    target: &FrameFitTarget,
    growth: &TextPosition,
    backgrounds: Vec<BackgroundGrowth>,
    fitted: &designcraft_engine::doc::Item,
    prefs: &Prefs,
    scratch: &mut Session,
    state_notes: (&mut ProductionPlan, &mut Vec<Value>),
) -> Result<(), String> {
    let (candidate, notes) = state_notes;
    let dy = growth.target.y0 - target.grown.y0;
    let has_later_geometry_op = candidate.commands.get(target.fit_index.saturating_add(1)..).is_some_and(|tail| {
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
    let needs_explicit_height_set = target.fit_is_existing_base || has_later_geometry_op || target.path_len > 1;
    let bg_cmd_count: usize = backgrounds
        .iter()
        .map(|bg| usize::from((bg.target.height() - bg.original.height()).abs() > 0.001) + usize::from((bg.target.y0 - bg.original.y0).abs() > 0.001))
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
            let height = (if prefs.dimensions_include_stroke { current.visible_bounds().height() } else { current.bounds().height() }) + bg_extra_h;
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
        notes.push(json!({"kind":"nativeBackgroundReflow","page":target.spread_index,"item":background.id.0,"extraHeightPt":bg_extra_h,"dy":bg_dy,
            "message":"Expanded a new rectangular text container into verified free space; preserved its padding and existing artwork"}));
    }
    if needs_explicit_height_set {
        let fitted_height =
            (if prefs.dimensions_include_stroke { fitted.visible_bounds().height() } else { fitted.bounds().height() }) + target.mark_pad;
        scratch.execute("transform.set", &json!({"ids":[target.fit_id.0],"height":fitted_height,"ref":0})).map_err(|error| error.to_string())?;
        candidate.commands.push(EngineAction {
            command: "transform.set".into(),
            params: json!({"ids":[target.fit_item_ref.clone()],"height":fitted_height,"ref":0}),
        });
    }
    if dy != 0.0 {
        scratch.execute("transform.move", &json!({"ids":[target.fit_id.0],"dx":0,"dy":dy})).map_err(|error| error.to_string())?;
        candidate
            .commands
            .push(EngineAction { command: "transform.move".into(), params: json!({"ids":[target.fit_item_ref.clone()],"dx":0,"dy":dy}) });
        notes.push(json!({"kind":"nativeTextTranslation","page":target.spread_index,"item":target.fit_id.0,"dx":0,"dy":dy,
            "message":"Moved newly fitted text upward into verified safe space; preserved copy, font sizes and explicit text insets"}));
    }
    if !needs_explicit_height_set {
        let rect = candidate
            .commands
            .get_mut(target.fit_index)
            .and_then(|a| a.params.get_mut("rect"))
            .and_then(Value::as_array_mut)
            .ok_or("Native text frame has no source rectangle")?;
        let bottom = rect.get(3).and_then(Value::as_f64).ok_or("Invalid native frame height")?;
        let value = rect.get_mut(3).ok_or("Invalid native frame rectangle")?;
        *value = json!(bottom + target.extra_height);
    }
    notes.push(json!({"kind":"nativeTextReflow","page":target.spread_index,"item":target.fit_id.0,"label":target.label,"extraHeightPt":target.extra_height,"message":"Expanded text frame into verified free space; preserved all text and font size"}));
    Ok(())
}

fn fit_single_new_story(
    entry: (usize, ItemId, StoryId),
    frame_sets: &CreatedFrameSets,
    base_prefs: (&DocState, &Prefs),
    scratch: &mut Session,
    candidate: &mut ProductionPlan,
    cancel: &AtomicBool,
    notes: &mut Vec<Value>,
) -> Result<(), String> {
    let (index, id, story_id) = entry;
    let (base, prefs) = base_prefs;
    let story_ref =
        frame_sets.frame_refs.get(&id).map(|(_, _, sr)| Value::String(sr.clone())).unwrap_or_else(|| Value::String(format!("${index}.story")));
    apply_story_glyph_fallbacks(story_id, id, &story_ref, scratch, candidate, notes)?;
    let doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
    let story = doc.story(story_id).ok_or("Native layout story disappeared")?;
    if !scratch.cache.get(&doc, story_id, None).is_overset() {
        return Ok(());
    }
    let target_id = doc.story(story_id).and_then(|st| st.frames.last().copied()).unwrap_or(id);
    let (fit_index, fit_id, fit_is_existing_base) = if let Some((idx, _, _)) = frame_sets.frame_refs.get(&target_id) {
        (*idx, target_id, false)
    } else if base.doc.item(target_id).is_some() {
        (index, target_id, true)
    } else {
        (index, id, base.doc.item(id).is_some())
    };
    let fit_item_ref = frame_sets.frame_refs.get(&fit_id).map(|(_, r, _)| Value::String(r.clone())).unwrap_or_else(|| json!(fit_id.0));
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
    let grown = Rect::new(measured.x0, measured.y0, measured.x1, measured.y1 + mark_pad);
    let allowed_loc = is_allowed_fit_location(&doc, &loc);
    let orig_w = if prefs.dimensions_include_stroke { item.visible_bounds().width() } else { item.bounds().width() };
    let orig_h = if prefs.dimensions_include_stroke { item.visible_bounds().height() } else { item.bounds().height() };
    let fit_target = FrameFitTarget {
        fit_index,
        fit_id,
        fit_is_existing_base,
        fit_item_ref,
        spread_index,
        page,
        old,
        grown,
        safe,
        mark_pad,
        extra_height,
        orig_w,
        orig_h,
        label: item.label.clone(),
        story_id,
        path_len: loc.path.len(),
    };
    let item_operable = allowed_loc
        && !item.locked
        && !item.hidden
        && !item.nonprinting
        && !doc.layer(item.layer).is_some_and(|layer| layer.locked || !layer.visible || !layer.printable);
    let mut growth = TextPosition { index: fit_index, id: fit_id, spread: spread_index, page, original: old, target: grown, safe, language: 0 };
    let mut blocked =
        !extra_height.is_finite() || extra_height <= 0.0 || !allowed_loc || (fit_is_existing_base && (loc.path.len() != 1 || !item_operable));
    // Height fitting happens after ordinary margin recovery. A new footer can
    // retain its bottom edge by moving only the measured excess upward.
    if !contains(safe, grown) {
        growth.target = shift(grown, 0.0, safe.y1 - grown.y1);
        blocked |= grown.y1 <= safe.y1
            || fit_is_existing_base
            || !item_operable
            || !contains(safe, growth.target)
            || position_collision(&growth, &BTreeMap::from([(fit_id, growth.target)]), &doc, candidate, cancel)?;
    }
    let mut backgrounds =
        if blocked { None } else { containing_background_growths(&growth, &doc, base, &frame_sets.created_frames, candidate, cancel)? };
    if backgrounds.is_none() && extra_height.is_finite() && extra_height > 0.0 && grown.y1 > old.y1 && !fit_is_existing_base && item_operable {
        let (env, all_obstacles) = ContainerEnvironment::collect(&fit_target, &doc, base, &frame_sets.created_frames, candidate, cancel)?;
        let (shift_candidates, min_allowed_dy) = build_upward_shift_candidates(&fit_target, &env, &all_obstacles, candidate);
        if let Some((up_growth, up_bgs)) = select_upward_shift_growth(
            &fit_target,
            &env,
            (&shift_candidates, min_allowed_dy),
            &doc,
            base,
            &frame_sets.created_frames,
            (candidate, cancel),
        )? {
            growth = up_growth;
            backgrounds = Some(up_bgs);
        }
        if backgrounds.is_none()
            && !story.text.trim().contains('\n')
            && story.text.chars().count() <= 64
            && story.text.split_whitespace().count() <= 10
            && old.height() <= 36.0
            && let Some(sol) = search_widened_label_solution(&fit_target, &env, &pre_fit_state, prefs, &doc, (candidate, cancel))?
        {
            return apply_widened_label_solution(&fit_target, sol, scratch, candidate, notes);
        }
    }
    let Some(backgrounds) = backgrounds.filter(|_| !scratch.cache.get(&fitted_doc, story_id, None).is_overset()) else {
        return Err(format!(
            "{identity} has overset text and needs {:.1} pt more height, but no safe free space remains; copy and font size were preserved. Reflow this activity before buying images",
            (measured.height() - old.height()).max(0.0)
        ));
    };
    apply_vertical_growth_commands(&fit_target, &growth, backgrounds, fitted, prefs, scratch, (candidate, notes))
}

fn fit_modified_existing_stories(
    seen_and_preexisting: (&BTreeSet<StoryId>, &BTreeSet<StoryId>),
    created_frames: &BTreeMap<ItemId, usize>,
    base_prefs: (&DocState, &Prefs),
    scratch: &mut Session,
    candidate: &mut ProductionPlan,
    cancel: &AtomicBool,
    notes: &mut Vec<Value>,
) -> Result<(), String> {
    let (seen, pre_existing_overset) = seen_and_preexisting;
    let (base, prefs) = base_prefs;
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
                apply_story_glyph_fallbacks(story_id, frame_id, &json!(story_id.0), scratch, candidate, notes)?;
            }
        }
        fit_single_existing_story(story_id, created_frames, (base, prefs), scratch, candidate, cancel, notes)?;
    }
    Ok(())
}

fn fit_single_existing_story(
    story_id: StoryId,
    created_frames: &BTreeMap<ItemId, usize>,
    base_prefs: (&DocState, &Prefs),
    scratch: &mut Session,
    candidate: &mut ProductionPlan,
    cancel: &AtomicBool,
    notes: &mut Vec<Value>,
) -> Result<(), String> {
    let (base, prefs) = base_prefs;
    let doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
    if !scratch.cache.get(&doc, story_id, None).is_overset() {
        return Ok(());
    }
    let story = doc.story(story_id).ok_or("Native layout story disappeared")?;
    let Some(&fit_id) = story.frames.last() else {
        return Ok(());
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
    let backgrounds = if blocked { None } else { containing_background_growths(&growth, &doc, base, created_frames, candidate, cancel)? };
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
    Ok(())
}

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
    let frame_sets = collect_created_and_text_frames(&candidate, results, &scratch.doc().map_err(|e| e.to_string())?.doc)?;
    notes.extend(recover_text_positions(&mut candidate, &mut scratch, base, &frame_sets.frames, &frame_sets.frame_refs, cancel)?);
    let mut seen = BTreeSet::new();
    for &entry in &frame_sets.frames {
        if cancel.load(Ordering::Relaxed) {
            return Err("Local layout cancelled".into());
        }
        if !seen.insert(entry.2) {
            continue;
        }
        fit_single_new_story(entry, &frame_sets, (base, prefs), &mut scratch, &mut candidate, cancel, &mut notes)?;
    }
    fit_modified_existing_stories(
        (&seen, &pre_existing_overset),
        &frame_sets.created_frames,
        (base, prefs),
        &mut scratch,
        &mut candidate,
        cancel,
        &mut notes,
    )?;
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

pub(super) fn check_repeated_slots(plan: &ProductionPlan, state: &DocState, cancel: &AtomicBool) -> Result<(), String> {
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

pub(super) fn frame_identity(spread: usize, index: usize, id: ItemId, rect: Rect, text: &str) -> String {
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

#[derive(Clone)]
pub(super) struct TextPosition {
    pub(super) index: usize,
    pub(super) id: ItemId,
    pub(super) spread: usize,
    pub(super) page: usize,
    pub(super) original: Rect,
    pub(super) target: Rect,
    pub(super) safe: Rect,
    pub(super) language: u8,
}

/// Only translate new, ungrouped, axis-aligned frames. Authored commands and their
/// indices stay intact; the exact private recovery is appended as explicit moves.
pub(super) fn recover_text_positions(
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
