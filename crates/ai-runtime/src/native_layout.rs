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
    let mut notes = format_populated_new_stories(&mut candidate)?;
    notes.extend(recover_unregistered_swatches(&mut candidate, base)?);
    crate::image_repetition::normalize(&mut candidate)?;
    crate::design::check_document_image_slots(base, &candidate.images)?;
    let mut scratch = Session::new();
    scratch.prefs = prefs.clone();
    scratch.add_document(base.clone());
    let result = scratch
        .execute("production.apply", &json!({"commands":candidate.commands,"outputMode":"native_with_images","requirePreflight":false}))
        .map_err(|e| e.to_string())?;
    let results = result.get("results").and_then(Value::as_array).ok_or("Native layout did not return command results")?;
    let mut frames = Vec::new();
    let mut created_frames = BTreeMap::new();
    for (index, action) in candidate.commands.iter().enumerate() {
        if action.command == "frame.create"
            && action.params.get("content").and_then(Value::as_str) == Some("unassigned")
            && let Some(id) = results.get(index).and_then(|value| value.get("id")).and_then(Value::as_u64)
        {
            created_frames.insert(ItemId(id), index);
        }
        if action.command == "frame.create" && action.params.get("content").and_then(Value::as_str) == Some("text") {
            let id = results.get(index).and_then(|v| v.get("id")).and_then(Value::as_u64).ok_or("Native text frame has no ID")?;
            let story = results.get(index).and_then(|v| v.get("story")).and_then(Value::as_u64).ok_or("Native text frame has no story")?;
            frames.push((index, ItemId(id), StoryId(story)));
        }
    }
    notes.extend(recover_text_positions(&mut candidate, &mut scratch, base, &frames, cancel)?);
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
        let old = outer.transform_rect_bbox(item.bounds());
        let identity = frame_identity(spread_index, index, id, old, &story.text);
        let linear = (outer * item.xf).as_coeffs();
        if (linear[0] - 1.0).abs() > 0.001 || linear[1].abs() > 0.001 || linear[2].abs() > 0.001 || (linear[3] - 1.0).abs() > 0.001 {
            return Err(format!("{identity} has overset text in a transformed frame; reflow its layout before generation"));
        }
        scratch.execute("object.fit", &json!({"ids":[id.0],"mode":"fitFrameToContent","axis":"height"})).map_err(|e| e.to_string())?;
        let fitted_doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
        let fitted = fitted_doc.item(id).ok_or("Fitted text frame disappeared")?;
        let measured = outer.transform_rect_bbox(fitted.bounds());
        let extra_height = measured.height() - old.height() + 0.01;
        // Include the same rounding allowance in collision checks and the published source.
        let grown = Rect::new(measured.x0, measured.y0, measured.x1, measured.y1 + 0.01);
        let page = spread.page_at_x(old.center().x).ok_or("Native text frame has no owning page")?;
        let safe = spread.pages.get(page).ok_or("Native text page disappeared")?.margin_rect();
        let mut growth = TextPosition { index, id, spread: spread_index, page, original: old, target: grown, safe, language: 0 };
        let mut blocked = !extra_height.is_finite() || extra_height <= 0.0;
        // Height fitting happens after ordinary margin recovery. A new footer can
        // retain its bottom edge by moving only the measured excess upward.
        if !contains(safe, grown) {
            growth.target = shift(grown, 0.0, safe.y1 - grown.y1);
            blocked |= grown.y1 <= safe.y1
                || loc.path.len() != 1
                || base.doc.item(id).is_some()
                || item.locked
                || item.hidden
                || item.nonprinting
                || doc.layer(item.layer).is_some_and(|layer| layer.locked || !layer.visible || !layer.printable)
                || !contains(safe, growth.target)
                || position_collision(&growth, &BTreeMap::from([(id, growth.target)]), &doc, &candidate, cancel)?;
        }
        let backgrounds = if blocked { None } else { containing_background_growths(&growth, &doc, base, &created_frames, &candidate, cancel)? };
        let Some(backgrounds) = backgrounds.filter(|_| !scratch.cache.get(&fitted_doc, story_id, None).is_overset()) else {
            return Err(format!(
                "{identity} has overset text and needs {:.1} pt more height, but no safe free space remains; copy and font size were preserved. Reflow this activity before buying images",
                (measured.height() - old.height()).max(0.0)
            ));
        };
        let dy = growth.target.y0 - grown.y0;
        if candidate
            .commands
            .len()
            .checked_add(backgrounds.len())
            .and_then(|length| length.checked_add(usize::from(dy != 0.0)))
            .is_none_or(|length| length > 512)
        {
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
        if dy != 0.0 {
            scratch.execute("transform.move", &json!({"ids":[id.0],"dx":0,"dy":dy})).map_err(|error| error.to_string())?;
            candidate
                .commands
                .push(EngineAction { command: "transform.move".into(), params: json!({"ids":[format!("${index}.id")],"dx":0,"dy":dy}) });
            notes.push(json!({"kind":"nativeTextTranslation","page":spread_index,"item":id.0,"dx":0,"dy":dy,
                "message":"Moved newly fitted text upward into verified safe space; preserved copy, font sizes and explicit text insets"}));
        }
        let rect = candidate
            .commands
            .get_mut(index)
            .and_then(|a| a.params.get_mut("rect"))
            .and_then(Value::as_array_mut)
            .ok_or("Native text frame has no source rectangle")?;
        let bottom = rect.get(3).and_then(Value::as_f64).ok_or("Invalid native frame height")?;
        let value = rect.get_mut(3).ok_or("Invalid native frame rectangle")?;
        *value = json!(bottom + extra_height);
        notes.push(json!({"kind":"nativeTextReflow","page":spread_index,"item":id.0,"label":item.label,"extraHeightPt":extra_height,"message":"Expanded text frame into verified free space; preserved all text and font size"}));
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
    let mut notes = Vec::new();
    for (index, action) in plan.commands.iter_mut().enumerate() {
        let field = match action.command.as_str() {
            "object.fill" | "object.stroke" => action.params.get_mut("swatch"),
            "type.char" => action.params.get_mut("attrs").and_then(|attrs| attrs.get_mut("fill")),
            "table.setCell" => action.params.get_mut("fill"),
            _ => None,
        };
        let Some(slot) = field else { continue };
        let Some(requested) = slot.as_str().map(str::to_owned) else { continue };
        if requested.is_empty() || requested.starts_with('$') || known.contains(&requested) {
            continue;
        }
        let requested_bare = bare(&requested);
        let replacement = if let Some(exact) = base.doc.swatches.iter().find(|s| bare(&s.name) == requested_bare) {
            Some(exact.name.clone())
        } else if requested_bare == "transparent"
            && let Some(none_swatch) = base.doc.swatches.iter().find(|s| matches!(s.value, designcraft_engine::doc::color::SwatchValue::None))
        {
            Some(none_swatch.name.clone())
        } else if let Some(target) = parse_inline_color(&requested) {
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
        };
        if let Some(resolved) = replacement {
            *slot = Value::String(resolved.clone());
            notes.push(json!({
                "kind": "nativeSwatchFallback",
                "command": index,
                "requested": requested,
                "resolved": resolved,
                "message": "Resolved unregistered swatch reference to an existing document swatch"
            }));
        }
    }
    Ok(notes)
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
    if let Some(hex) = trimmed.strip_prefix('#') {
        if hex.len() == 6
            && let (Ok(r), Ok(g), Ok(b)) =
                (u8::from_str_radix(&hex[0..2], 16), u8::from_str_radix(&hex[2..4], 16), u8::from_str_radix(&hex[4..6], 16))
        {
            return Some(Color::rgb8(r, g, b));
        }
        if hex.len() == 3
            && let (Ok(r), Ok(g), Ok(b)) =
                (u8::from_str_radix(&hex[0..1], 16), u8::from_str_radix(&hex[1..2], 16), u8::from_str_radix(&hex[2..3], 16))
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

/// Grow only containers whose authorship, geometry and complete new bottom band
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
            || loc.path.len() != 1
            || item.shape != designcraft_engine::doc::Shape::Rectangle
            || !matches!(item.content, designcraft_engine::doc::Content::Unassigned)
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
        let target =
            Rect::new(obstacle.bounds.x0, obstacle.bounds.y0, obstacle.bounds.x1, obstacle.bounds.y1 + text.target.height() - text.original.height());
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
        let band = Rect::new(background.target.x0, background.original.y1, background.target.x1, background.target.y1);
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
}
