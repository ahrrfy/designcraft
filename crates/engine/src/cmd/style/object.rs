use super::{Effects, Item, ObjectStyle, Result, Session, Styles, Value, bad, json, str_param};

/// Does setting `name`'s `based_on` to `parent` create a cycle (including self-reference or an
/// existing cycle reachable from `parent`)?
pub(crate) fn object_based_on_cycles(styles: &Styles, name: &str, parent: &str) -> bool {
    if name == parent {
        return true;
    }
    let mut seen: Vec<&str> = vec![name];
    let mut cur = Some(parent);
    for _ in 0..=styles.object.len() {
        let Some(p) = cur else { return false };
        if seen.contains(&p) {
            return true;
        }
        seen.push(p);
        cur = styles.object_style(p).and_then(|s| s.based_on.as_deref());
    }
    true
}

/// Resolve an object style along its `based_on` chain (root to leaf, bounded against cycles in
/// imported documents).
pub(crate) fn resolved_object_style(styles: &Styles, name: &str) -> Option<ObjectStyle> {
    styles.resolve_object_style(name)
}

pub(super) fn apply_object(s: &mut Session, p: &Value) -> Result<Value> {
    let name = str_param(p, "name").ok_or_else(|| bad("style.object.apply", "missing name"))?.to_string();
    let os = resolved_object_style(&s.doc()?.doc.styles, &name).ok_or_else(|| bad("style.object.apply", format!("no object style `{name}`")))?;
    let ids = crate::cmd::targets(s, p)?;
    s.edit(|d, _| {
        let mut stories = Vec::new();
        for id in &ids {
            let Some(it) = d.item_mut(*id) else { continue };
            it.object_style = name.clone();
            os.apply_to(it);
            if let Some(tf) = it.text_frame() {
                stories.push(tf.story);
            }
        }
        if let Some(ps) = &os.paragraph_style {
            for sid in stories {
                if let Some(st) = d.story_mut(sid) {
                    let len = st.len();
                    st.format_paras(0..len, |f| f.style = ps.clone());
                }
            }
        }
        Ok(Value::Null)
    })
}

pub(super) fn clear_object_overrides(s: &mut Session, p: &Value) -> Result<Value> {
    let ids = crate::cmd::targets(s, p)?;
    // Each target with its style's attributes applied, kept only where that changes the item.
    let d = &s.doc()?.doc;
    let changed: Vec<Item> = ids
        .iter()
        .filter_map(|id| {
            let it = d.item(*id)?;
            let mut next = it.clone();
            resolved_object_style(&d.styles, &it.object_style)?.apply_to(&mut next);
            (next != *it).then_some(next)
        })
        .collect();
    if changed.is_empty() {
        return Ok(json!({"changed": 0}));
    }
    s.edit(|d, _| {
        for next in &changed {
            if let Some(it) = d.item_mut(next.id) {
                *it = next.clone();
            }
        }
        Ok(json!({"changed": changed.len()}))
    })
}

/// Effect keys accepted at the top level of `style.object.create` / `edit` as shorthand for
/// `effects.<key>`.
const EFFECT_KEYS: [&str; 9] =
    ["dropShadow", "feather", "innerShadow", "outerGlow", "innerGlow", "bevel", "satin", "gradientFeather", "directionalFeather"];

/// The effects `p` gives (`effects`, then the top-level effect keys) merged field by field over
/// `base`, or `None` when `p` names no effect.
fn effects_param(cmd: &str, p: &Value, base: &Effects) -> Result<Option<Effects>> {
    let mut patch = serde_json::Map::new();
    match p.get("effects") {
        None | Some(Value::Null) => {}
        Some(Value::Object(o)) => patch.extend(o.iter().map(|(k, v)| (k.clone(), v.clone()))),
        Some(_) => return Err(bad(cmd, "`effects` must be an object or null")),
    }
    for k in EFFECT_KEYS {
        if let Some(v) = p.get(k) {
            patch.insert(k.to_string(), v.clone());
        }
    }
    if patch.is_empty() {
        return Ok(None);
    }
    if patch
        .get("gradientFeather")
        .and_then(Value::as_object)
        .is_some_and(|gf| gf.contains_key("from") || gf.contains_key("to") || gf.get("vector").is_some_and(|v| !v.is_null()))
    {
        return Err(bad(cmd, "object styles cannot store spread-space gradientFeather endpoints; use angle"));
    }
    let mut v = serde_json::to_value(base).map_err(|e| bad(cmd, e.to_string()))?;
    merge_json(&mut v, Value::Object(patch), 0);
    let e: Effects = serde_json::from_value(v).map_err(|e| bad(cmd, format!("effects: {e}")))?;
    e.sanitized().map(Some).map_err(|m| bad(cmd, m))
}

/// Merges `top` into `base`: objects key by key, anything else replaced.
fn merge_json(base: &mut Value, top: Value, depth: usize) {
    match (base, top) {
        (Value::Object(b), Value::Object(t)) if depth < 8 => {
            for (k, v) in t {
                match b.get_mut(&k) {
                    Some(slot) => merge_json(slot, v, depth + 1),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, t) => *b = t,
    }
}

pub(super) fn create_object(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "style.object.create";
    let base = str_param(p, "name").unwrap_or("Object Style 1").to_string();
    let based_on = match p.get("basedOn") {
        None | Some(Value::Null) => None,
        Some(Value::String(b)) if b == designcraft_doc::NO_OBJECT_STYLE => None,
        Some(Value::String(b)) => Some(b.clone()),
        Some(_) => return Err(bad(CMD, "`basedOn` must be a string or null")),
    };
    let from_sel = p.get("fromSelection").and_then(Value::as_bool).unwrap_or(based_on.is_none());
    let src = if from_sel { s.doc()?.selection.items.first().and_then(|i| s.doc().ok()?.doc.item(*i).cloned()) } else { None };
    let mut os = ObjectStyle::default();
    if let Some(it) = &src {
        os.fill = Some(it.fill.clone());
        os.stroke = Some(it.stroke.clone());
        os.text_frame = it.text_frame().map(|t| t.options.clone());
        let mut fx = it.effects.clone();
        if let Some([x0, y0, x1, y1]) = fx.gradient_feather.vector.take()
            && !fx.gradient_feather.radial
        {
            let (dx, dy) = (x1 - x0, y1 - y0);
            if dx.is_finite() && dy.is_finite() && dx.hypot(dy) > 1e-6 {
                fx.gradient_feather.angle = (-dy).atan2(dx).to_degrees();
            }
        }
        os.effects = Some(fx);
    }
    if let Some(sw) = str_param(p, "fill") {
        os.fill = Some(designcraft_doc::Fill::swatch(sw));
    }
    if let Some(ps) = str_param(p, "paragraphStyle") {
        os.paragraph_style = Some(ps.to_string());
    }
    if let Some(e) = effects_param(CMD, p, os.effects.as_ref().unwrap_or(&Effects::default()))? {
        os.effects = Some(e);
    }
    s.edit(|d, _| {
        let name = Styles::unique_name(|n| d.styles.object_style(n).is_some(), &base);
        if let Some(b) = &based_on {
            if d.styles.object_style(b).is_none() {
                return Err(bad(CMD, format!("no object style `{b}`")));
            }
            if object_based_on_cycles(&d.styles, &name, b) {
                return Err(bad(CMD, "based-on would create a cycle"));
            }
        }
        os.name = name.clone();
        os.based_on = based_on.clone();
        d.styles_mut().object.push(os.clone());
        Ok(json!({"name": name}))
    })
}

pub(super) fn edit_object(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "style.object.edit";
    let name = str_param(p, "name").ok_or_else(|| bad(CMD, "missing name"))?.to_string();
    if name == designcraft_doc::NO_OBJECT_STYLE {
        return Err(bad(CMD, format!("`{name}` can't be edited")));
    }
    let old = s.doc()?.doc.styles.object_style(&name).cloned().ok_or_else(|| bad(CMD, format!("no object style `{name}`")))?;
    let based_on_change = match p.get("basedOn") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(b)) if b == designcraft_doc::NO_OBJECT_STYLE => Some(None),
        Some(Value::String(b)) => {
            let doc = &s.doc()?.doc;
            if doc.styles.object_style(b).is_none() {
                return Err(bad(CMD, format!("no object style `{b}`")));
            }
            if object_based_on_cycles(&doc.styles, &name, b) {
                return Err(bad(CMD, "based-on would create a cycle"));
            }
            Some(Some(b.clone()))
        }
        Some(_) => return Err(bad(CMD, "`basedOn` must be a string or null")),
    };
    let (effects_changed, effects) = if p.get("effects").is_some_and(Value::is_null) {
        if EFFECT_KEYS.iter().any(|k| p.get(k).is_some()) {
            return Err(bad(CMD, "`effects: null` can't be combined with effect settings"));
        }
        (old.effects.is_some(), None)
    } else {
        match effects_param(CMD, p, old.effects.as_ref().unwrap_or(&Effects::default()))? {
            Some(e) => (Some(&e) != old.effects.as_ref(), Some(e)),
            None => (false, old.effects.clone()),
        }
    };
    let based_changed = based_on_change.as_ref().is_some_and(|next| *next != old.based_on);
    if !effects_changed && !based_changed {
        return Ok(json!({"name": name, "updated": 0}));
    }
    s.edit(|d, _| {
        let before_fx: Vec<(String, Option<Effects>)> =
            d.styles.object.iter().map(|o| (o.name.clone(), resolved_object_style(&d.styles, &o.name).and_then(|s| s.effects))).collect();
        let Some(os) = d.styles_mut().object.iter_mut().find(|o| o.name == name) else {
            return Err(bad(CMD, format!("no object style `{name}`")));
        };
        if let Some(next_based) = &based_on_change {
            os.based_on = next_based.clone();
        }
        os.effects = effects.clone();
        // An item follows its style while its effects match the style's resolved effects (or no
        // effects, when the style previously resolved to none). Propagates through `based_on`.
        let mut updated = 0;
        for (st_name, old_resolved) in before_fx {
            let new_resolved = resolved_object_style(&d.styles, &st_name).and_then(|s| s.effects);
            if let Some(next_fx) = &new_resolved
                && new_resolved != old_resolved
            {
                let follows = old_resolved.unwrap_or_default();
                updated += d.update_items(|it| it.object_style == st_name && it.effects == follows, |it| it.effects = next_fx.clone());
            }
        }
        Ok(json!({"name": name, "updated": updated}))
    })
}
