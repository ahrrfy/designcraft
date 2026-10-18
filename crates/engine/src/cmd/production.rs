//! Restricted, native AI document production. Plans execute on a private draft; only a complete
//! successful draft is published. Neither provider output nor a caller's allowlist grants IO.

use serde_json::{Value, json};

use super::{CommandSpec, always, bad, cmd, has_doc};
use crate::{HistoryEntry, Result, Session};

const ID: &str = "production.apply";
const MAX_ACTIONS: usize = 512;
const MAX_BYTES: usize = 16 * 1024 * 1024;

fn object(fields: &[(&str, Value)], required: &[&str]) -> Value {
    json!({"type":"object", "properties": fields.iter().map(|(k,v)| (k.to_string(),v.clone())).collect::<serde_json::Map<_,_>>(),
        "required":required,"additionalProperties":false})
}
fn number(min: f64, max: f64) -> Value {
    json!({"type":"number","minimum":min,"maximum":max})
}
fn integer(min: u64, max: u64) -> Value {
    json!({"type":"integer","minimum":min,"maximum":max})
}
fn string() -> Value {
    json!({"type":"string","maxLength":262144})
}
fn boolean() -> Value {
    json!({"type":"boolean"})
}
fn choice(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}
fn array(items: Value, min: usize, max: usize) -> Value {
    json!({"type":"array","items":items,"minItems":min,"maxItems":max})
}
fn ids() -> Value {
    array(integer(1, u64::MAX), 1, 1024)
}
fn rect() -> Value {
    array(number(-15552.0, 15552.0), 4, 4)
}
fn point() -> Value {
    array(number(-15552.0, 15552.0), 2, 2)
}

/// Vetted parameter schemas. Deliberately narrower than the general command interface.
fn schemas() -> Vec<(&'static str, Value, Value)> {
    let dim = || number(0.01, 15552.0);
    let positive = || number(0.01, 1000.0);
    let index = || integer(0, 999);
    let edges = || json!({"anyOf":[number(0.0,15552.0),array(number(0.0,15552.0),4,4)]});
    vec![
        (
            "frame.create",
            object(
                &[
                    ("rect", rect()),
                    ("spread", index()),
                    ("shape", choice(&["rectangle", "ellipse", "polygon"])),
                    ("content", choice(&["text", "graphic", "unassigned"])),
                    ("text", string()),
                    ("caret", boolean()),
                    ("vertical", boolean()),
                    ("sides", integer(3, 64)),
                    ("starInset", number(0.0, 100.0)),
                ],
                &["rect"],
            ),
            json!({"rect":[54,54,540,150],"content":"text","text":"عنوان الدرس","caret":false}),
        ),
        ("line.create", object(&[("a", point()), ("b", point()), ("spread", index())], &["a", "b"]), json!({"a":[54,160],"b":[540,160]})),
        (
            "path.create",
            object(
                &[
                    ("spread", index()),
                    ("closed", boolean()),
                    ("anchors", array(object(&[("p", point()), ("in", point()), ("out", point())], &["p"]), 2, 1024)),
                ],
                &["anchors"],
            ),
            json!({"anchors":[{"p":[54,54]},{"p":[88,54]},{"p":[71,78]}],"closed":true}),
        ),
        ("object.label", object(&[("ids", ids()), ("label", string())], &["ids", "label"]), json!({"ids":["$0.id"],"label":"lesson.heading"})),
        ("selection.set", object(&[("ids", array(integer(1, u64::MAX), 0, 1024)), ("add", boolean())], &["ids"]), json!({"ids":["$0.id"]})),
        (
            "object.fill",
            object(&[("ids", ids()), ("swatch", string()), ("tint", number(0.0, 1.0))], &["ids", "swatch"]),
            json!({"ids":["$0.id"],"swatch":"[Paper]"}),
        ),
        (
            "object.stroke",
            object(
                &[
                    ("ids", ids()),
                    ("swatch", string()),
                    ("weight", number(0.0, 1000.0)),
                    ("tint", number(0.0, 1.0)),
                    ("align", choice(&["center", "inside", "outside"])),
                    ("cap", choice(&["butt", "round", "projecting"])),
                    ("join", choice(&["miter", "round", "bevel"])),
                ],
                &["ids"],
            ),
            json!({"ids":["$0.id"],"swatch":"[Black]","weight":1}),
        ),
        (
            "object.arrange",
            object(&[("ids", ids()), ("to", choice(&["front", "forward", "backward", "back"]))], &["ids", "to"]),
            json!({"ids":["$0.id"],"to":"front"}),
        ),
        ("object.group", object(&[("ids", ids())], &["ids"]), json!({"ids":["$0.id","$1.id"]})),
        (
            "object.textFrameOptions",
            object(
                &[
                    ("ids", ids()),
                    ("columns", integer(1, 64)),
                    ("gutter", number(0.0, 1000.0)),
                    ("inset", edges()),
                    ("verticalJustification", choice(&["top", "center", "bottom", "justify"])),
                    ("ignoreWrap", boolean()),
                    ("balanceColumns", boolean()),
                ],
                &["ids"],
            ),
            json!({"ids":["$0.id"],"inset":6}),
        ),
        (
            "transform.move",
            object(&[("ids", ids()), ("dx", number(-15552.0, 15552.0)), ("dy", number(-15552.0, 15552.0))], &["ids", "dx", "dy"]),
            json!({"ids":["$0.id"],"dx":0,"dy":20}),
        ),
        (
            "transform.set",
            object(
                &[
                    ("ids", ids()),
                    ("x", number(-15552.0, 15552.0)),
                    ("y", number(-15552.0, 15552.0)),
                    ("width", dim()),
                    ("height", dim()),
                    ("rotation", number(-360.0, 360.0)),
                    ("ref", integer(0, 8)),
                ],
                &["ids"],
            ),
            json!({"ids":["$0.id"],"x":54,"y":54,"ref":0}),
        ),
        (
            "text.select",
            object(
                &[("story", integer(1, u64::MAX)), ("anchor", integer(0, 16777216)), ("focus", integer(0, 16777216))],
                &["story", "anchor", "focus"],
            ),
            json!({"story":"$0.story","anchor":0,"focus":0}),
        ),
        ("text.insert", object(&[("text", string()), ("raw", boolean())], &["text"]), json!({"text":"نص قابل للتحرير"})),
        (
            "story.setText",
            object(&[("story", integer(1, u64::MAX)), ("text", string())], &["story", "text"]),
            json!({"story":"$0.story","text":"نص الدرس"}),
        ),
        (
            "story.setDirection",
            object(&[("story", integer(1, u64::MAX)), ("direction", choice(&["leftToRight", "rightToLeft"]))], &["story", "direction"]),
            json!({"story":"$0.story","direction":"rightToLeft"}),
        ),
        (
            "type.char",
            object(
                &[(
                    "attrs",
                    object(
                        &[
                            ("fontFamily", string()),
                            ("fontStyle", string()),
                            ("language", string()),
                            ("size", positive()),
                            ("fill", string()),
                            ("tracking", number(-1000.0, 1000.0)),
                            ("underline", boolean()),
                            (
                                "leading",
                                json!({"anyOf":[object(&[("kind",choice(&["auto"]))], &["kind"]),object(&[("kind",choice(&["points"])),("value",positive())], &["kind","value"])]}),
                            ),
                        ],
                        &[],
                    ),
                )],
                &["attrs"],
            ),
            json!({"attrs":{"size":14,"fontFamily":"Arial"}}),
        ),
        (
            "type.para",
            object(
                &[(
                    "attrs",
                    object(
                        &[
                            ("align", choice(&["left", "center", "right", "leftJustified", "rightJustified", "centerJustified", "fullyJustified"])),
                            ("direction", choice(&["leftToRight", "rightToLeft"])),
                            ("leftIndent", number(0.0, 1000.0)),
                            ("rightIndent", number(0.0, 1000.0)),
                            ("firstLineIndent", number(-1000.0, 1000.0)),
                            ("spaceBefore", number(0.0, 1000.0)),
                            ("spaceAfter", number(0.0, 1000.0)),
                            ("hyphenate", boolean()),
                        ],
                        &[],
                    ),
                )],
                &["attrs"],
            ),
            json!({"attrs":{"align":"right","direction":"rightToLeft"}}),
        ),
        (
            "swatch.create",
            object(
                &[
                    ("name", string()),
                    (
                        "color",
                        json!({"anyOf":[json!({"type":"string","maxLength":7}),object(&[("c",number(0.0,100.0)),("m",number(0.0,100.0)),("y",number(0.0,100.0)),("k",number(0.0,100.0))], &["c","m","y","k"])]}),
                    ),
                    ("spot", boolean()),
                ],
                &["color"],
            ),
            json!({"name":"School blue","color":{"c":100,"m":60,"y":0,"k":0}}),
        ),
        (
            "layout.documentSetup",
            object(
                &[
                    ("width", dim()),
                    ("height", dim()),
                    ("pages", integer(1, 1000)),
                    ("margins", number(0.0, 15552.0)),
                    ("bleed", edges()),
                    ("slug", edges()),
                    ("facingPages", boolean()),
                    ("binding", choice(&["leftToRight", "rightToLeft"])),
                    ("intent", choice(&["print", "web", "mobile"])),
                    ("adjustLayout", boolean()),
                ],
                &[],
            ),
            json!({"width":595.2756,"height":841.8898,"margins":54,"facingPages":false}),
        ),
        ("layout.pages.insert", object(&[("count", integer(1, 100)), ("after", index())], &["count"]), json!({"count":1})),
        (
            "layout.marginsAndColumns",
            object(
                &[
                    ("pages", array(index(), 1, 1000)),
                    ("margins", number(0.0, 15552.0)),
                    ("columns", integer(1, 64)),
                    ("gutter", number(0.0, 1000.0)),
                    ("adjustLayout", boolean()),
                ],
                &[],
            ),
            json!({"margins":54,"columns":1}),
        ),
        (
            "table.insert",
            object(
                &[
                    ("rows", integer(1, 100)),
                    ("cols", integer(1, 32)),
                    ("headerRows", integer(0, 10)),
                    ("footerRows", integer(0, 10)),
                    ("width", dim()),
                ],
                &["rows", "cols"],
            ),
            json!({"rows":4,"cols":3,"width":500}),
        ),
        (
            "table.setCell",
            object(
                &[
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("row", index()),
                    ("col", integer(0, 31)),
                    ("text", string()),
                    ("fill", string()),
                    ("tint", number(0.0, 1.0)),
                    ("insets", edges()),
                    ("verticalJustification", choice(&["top", "center", "bottom", "justify"])),
                ],
                &["row", "col"],
            ),
            json!({"row":0,"col":0,"text":"الاسم"}),
        ),
        (
            "file.place",
            object(
                &[
                    ("base64", json!({"type":"string","maxLength":8388608})),
                    ("name", json!({"type":"string","maxLength":256})),
                    ("frame", integer(1, u64::MAX)),
                    ("spread", index()),
                    ("x", number(-15552.0, 15552.0)),
                    ("y", number(-15552.0, 15552.0)),
                    ("width", dim()),
                ],
                &["base64", "name"],
            ),
            json!({"base64":"<PNG or JPEG base64>","name":"illustration.png","x":54,"y":200,"width":200}),
        ),
    ]
}

/// Catalog consumed by prompt builders and the AI permissions UI.
pub fn catalog() -> Value {
    let commands: Vec<_> = schemas()
        .into_iter()
        .map(|(id, schema, example)| {
            json!({"command":id,"paramsSchema":schema,"example":example,
        "description":super::find_command(id).map(|s| s.params),"requiresImages":id=="file.place"})
        })
        .collect();
    json!({"version":1,"outputModes":["native_only","native_with_images"],"maxActions":MAX_ACTIONS,
        "references":"$N.path and ${N.path}; zero-based earlier results, resolved values are revalidated",
        "commands":commands})
}

fn check(schema: &Value, value: &Value, path: &str) -> Result<()> {
    if let Some(options) = schema.get("anyOf").and_then(Value::as_array) {
        return if options.iter().any(|s| check(s, value, path).is_ok()) {
            Ok(())
        } else {
            Err(bad(ID, format!("{path}: no permitted type matches")))
        };
    }
    let valid = match schema.get("type").and_then(Value::as_str) {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("boolean") => value.is_boolean(),
        Some("integer") => value.as_u64().is_some(),
        Some("number") => value.as_f64().is_some_and(f64::is_finite),
        _ => false,
    };
    if !valid {
        return Err(bad(ID, format!("{path}: invalid parameter type")));
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        return Err(bad(ID, format!("{path}: unsupported value")));
    }
    if let Some(n) = value.as_f64()
        && (schema.get("minimum").and_then(Value::as_f64).is_some_and(|min| n < min)
            || schema.get("maximum").and_then(Value::as_f64).is_some_and(|max| n > max))
    {
        return Err(bad(ID, format!("{path}: outside permitted range")));
    }
    if let Some(text) = value.as_str()
        && schema.get("maxLength").and_then(Value::as_u64).is_some_and(|max| text.chars().count() as u64 > max)
    {
        return Err(bad(ID, format!("{path}: text too long")));
    }
    if let Some(values) = value.as_array() {
        if schema.get("minItems").and_then(Value::as_u64).is_some_and(|min| (values.len() as u64) < min)
            || schema.get("maxItems").and_then(Value::as_u64).is_some_and(|max| values.len() as u64 > max)
        {
            return Err(bad(ID, format!("{path}: invalid array length")));
        }
        let item = schema.get("items").ok_or_else(|| bad(ID, "missing item schema"))?;
        for v in values {
            check(item, v, path)?;
        }
    }
    if let Some(values) = value.as_object() {
        let props = schema.get("properties").and_then(Value::as_object).ok_or_else(|| bad(ID, "missing object schema"))?;
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !values.contains_key(key) {
                    return Err(bad(ID, format!("{path}: missing `{key}`")));
                }
            }
        }
        for (key, v) in values {
            check(props.get(key).ok_or_else(|| bad(ID, format!("{path}: unsupported parameter `{key}`")))?, v, &format!("{path}.{key}"))?;
        }
    }
    Ok(())
}

// Iterative walk bounds recursion before calling the script reference resolver or cloning JSON.
fn bounded(value: &Value) -> Result<()> {
    let mut stack = vec![(value, 0usize)];
    let mut bytes = 0usize;
    let mut nodes = 0usize;
    while let Some((v, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if depth > 24 || nodes > 65536 {
            return Err(bad(ID, "plan nesting or node limit exceeded"));
        }
        match v {
            Value::String(s) => bytes = bytes.saturating_add(s.len()),
            Value::Array(a) => {
                if a.len() > 65536 {
                    return Err(bad(ID, "array too large"));
                }
                stack.extend(a.iter().map(|x| (x, depth + 1)));
            }
            Value::Object(o) => {
                if o.len() > 128 {
                    return Err(bad(ID, "object too large"));
                }
                for (key, x) in o {
                    bytes = bytes.saturating_add(key.len());
                    stack.push((x, depth + 1));
                }
            }
            _ => {}
        }
        if bytes > MAX_BYTES {
            return Err(bad(ID, "plan byte limit exceeded"));
        }
    }
    Ok(())
}

fn draft(s: &Session, p: &Value, mut cancelled: impl FnMut() -> bool) -> Result<(crate::DocState, Vec<Value>)> {
    bounded(p)?;
    let params = p.as_object().ok_or_else(|| bad(ID, "parameters must be an object"))?;
    for key in params.keys() {
        if !["commands", "outputMode", "allowedCommands", "label", "expectedDocumentUid", "expectedRevision", "requirePreflight", "minPpi"]
            .contains(&key.as_str())
        {
            return Err(bad(ID, format!("unknown parameter `{key}`")));
        }
    }
    let require_preflight = match p.get("requirePreflight") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| bad(ID, "requirePreflight must be a boolean"))?,
    };
    let min_ppi = match p.get("minPpi") {
        None => 150.0,
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite() && (1.0..=2400.0).contains(n))
            .ok_or_else(|| bad(ID, "minPpi must be a finite number from 1 to 2400"))?,
    };
    let mode = p.get("outputMode").and_then(Value::as_str).ok_or_else(|| bad(ID, "outputMode required"))?;
    let images = match mode {
        "native_only" | "nativeOnly" => false,
        "native_with_images" | "nativeWithImages" => true,
        _ => return Err(bad(ID, "invalid outputMode")),
    };
    let actions = p
        .get("commands")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty() && a.len() <= MAX_ACTIONS)
        .ok_or_else(|| bad(ID, "commands must contain 1 to 512 actions"))?;
    let allowed = match p.get("allowedCommands") {
        None => None,
        Some(Value::Array(a)) if a.iter().all(Value::is_string) && a.len() <= 128 => Some(a),
        _ => return Err(bad(ID, "allowedCommands must be a string array")),
    };
    if let Some(label) = p.get("label")
        && !label.as_str().is_some_and(|s| !s.trim().is_empty() && s.len() <= 256)
    {
        return Err(bad(ID, "label must contain 1 to 256 bytes"));
    }
    let original = s.doc()?;
    if original.interaction.is_some() || original.preview_stash.is_some() || s.tool.composing() {
        return Err(bad(ID, "finish the current interaction or preview first"));
    }
    for (key, actual) in [("expectedDocumentUid", original.uid), ("expectedRevision", original.revision)] {
        if let Some(v) = p.get(key)
            && v.as_u64() != Some(actual)
        {
            return Err(bad(ID, format!("{key}: document changed while generation was pending")));
        }
    }
    let vetted = schemas();
    // Reject every command and malformed envelope before executing the first draft action.
    for action in actions {
        let a = action.as_object().ok_or_else(|| bad(ID, "each action must be an object"))?;
        if a.keys().any(|k| k != "command" && k != "params") {
            return Err(bad(ID, "unsupported action property"));
        }
        let id = action.get("command").and_then(Value::as_str).ok_or_else(|| bad(ID, "action command required"))?;
        if !vetted.iter().any(|(c, _, _)| *c == id)
            || (!images && id == "file.place")
            || allowed.is_some_and(|a| !a.iter().any(|c| c.as_str() == Some(id)))
        {
            return Err(bad(ID, format!("command `{id}` is not permitted")));
        }
        if !action.get("params").is_some_and(Value::is_object) {
            return Err(bad(ID, "action params must be an object"));
        }
    }
    let mut work = Session::new();
    work.cache = s.cache.clone();
    work.prefs = s.prefs.clone();
    work.docs.push(original.clone());
    work.active = Some(0);
    // A draft needs no per-action history or journal retention.
    work.in_undo_step = true;
    let mut results = Vec::with_capacity(actions.len());
    let mut image_pixels = 0u64;
    let mut table_cells = 0u64;
    for (i, action) in actions.iter().enumerate() {
        if cancelled() {
            return Err(bad(ID, "production cancelled"));
        }
        let id = action.get("command").and_then(Value::as_str).ok_or_else(|| bad(ID, "missing command"))?;
        let raw = action.get("params").ok_or_else(|| bad(ID, "missing params"))?;
        let resolved = crate::script::resolve(raw, &results).map_err(|e| bad(ID, format!("step {i}: {e}")))?;
        bounded(&resolved)?;
        let schema = vetted.iter().find(|(c, _, _)| *c == id).ok_or_else(|| bad(ID, "command not vetted"))?;
        check(&schema.1, &resolved, id)?;
        if id == "frame.create" {
            let coordinates: Vec<_> = resolved.get("rect").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_f64).collect();
            if coordinates.get(2).zip(coordinates.first()).is_none_or(|(right, left)| right <= left)
                || coordinates.get(3).zip(coordinates.get(1)).is_none_or(|(bottom, top)| bottom <= top)
            {
                return Err(bad(ID, "frame rectangle must have positive width and height"));
            }
        }
        if id == "file.place" {
            image_pixels = image_pixels.saturating_add(check_image(&resolved)?);
            if image_pixels > 32_000_000 {
                return Err(bad(ID, "plan exceeds 32 million placed image pixels"));
            }
        }
        if id == "table.insert" {
            let n = |key| resolved.get(key).and_then(Value::as_u64).unwrap_or(0);
            table_cells =
                table_cells.saturating_add(n("rows").saturating_add(n("headerRows")).saturating_add(n("footerRows")).saturating_mul(n("cols")));
            if table_cells > 10000 {
                return Err(bad(ID, "plan exceeds 10000 table cells"));
            }
        }
        let result = work.execute(id, &resolved).map_err(|e| bad(ID, format!("step {i} ({id}): {e}")))?;
        if work.doc()?.doc.page_count() > 1000 {
            return Err(bad(ID, "production page limit exceeded"));
        }
        work.journal.clear();
        results.push(result);
    }
    if require_preflight {
        check_text_layout(s, &work)?;
        // Gate the private draft before publication. Missing fonts/glyphs, overset text and
        // other error-level defects cannot leave a partially accepted live document behind.
        let issues = super::preflight::check(&work, min_ppi);
        let errors: Vec<_> = issues.iter().filter(|issue| issue.severity == "error").collect();
        if !errors.is_empty() {
            let summary = errors.iter().take(8).map(|issue| format!("{}: {}", issue.kind, issue.message)).collect::<Vec<_>>().join("; ");
            return Err(bad(ID, format!("preflight rejected draft ({} errors): {summary}", errors.len())));
        }
    }
    if cancelled() {
        return Err(bad(ID, "production cancelled"));
    }
    Ok((work.doc()?.clone(), results))
}

/// A rendered document-page instance. A parent frame can have an instance on several pages.
struct LayoutFrame<'a> {
    item: &'a designcraft_doc::Item,
    story: &'a std::sync::Arc<designcraft_doc::Story>,
    parent: designcraft_geom::Affine,
    bounds: designcraft_geom::Rect,
    safe: designcraft_geom::Rect,
    page: usize,
    page_id: u64,
}

fn layout_frames(s: &Session) -> Result<Vec<LayoutFrame<'_>>> {
    use designcraft_doc::{ItemLoc, SpreadRef};
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
    // Location, page-placement transform, forced owning page for inherited parent items,
    // and inherited visibility. Match the renderer's parent-page placement and overrides.
    let mut pending = Vec::new();
    for (si, sp) in d.spreads.iter().enumerate() {
        let sr = SpreadRef::Doc(si);
        for (index, _) in sp.items.iter().enumerate() {
            if pending.len() >= 65536 {
                return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
            }
            pending.push((ItemLoc { spread: sr, path: vec![index] }, Affine::IDENTITY, None, true));
        }
        for (pi, page) in sp.pages.iter().enumerate().filter(|(_, page)| page.show_parent_items) {
            for (ppi, ppage) in d.shown_parent_pages(sr, pi) {
                let Some(parent) = d.parents.get(ppi) else { continue };
                let Some(ppg) = parent.pages.get(ppage) else { continue };
                for (index, item) in parent.items.iter().enumerate() {
                    if page.overridden.contains(&item.id) || (parent.pages.len() > 1 && parent.page_at_x(item.bounds().center().x) != Some(ppage)) {
                        continue;
                    }
                    if pending.len() >= 65536 {
                        return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
                    }
                    pending.push((
                        ItemLoc { spread: SpreadRef::Parent(ppi), path: vec![index] },
                        Affine::translate((page.x - ppg.x, 0.0)),
                        Some(d.first_page_of_spread(si) + pi),
                        true,
                    ));
                }
            }
        }
    }
    if pending.len() > 65536 {
        return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
    }
    let mut frames = Vec::new();
    let mut visited = 0usize;
    while let Some((loc, placement, forced_page, inherited)) = pending.pop() {
        visited = visited.saturating_add(1);
        if visited > 65536 || pending.len() > 65536 {
            return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
        }
        let item = d.item_at(&loc).ok_or_else(|| bad(ID, "textLayoutLocation: invalid item location"))?;
        let visible = inherited
            && !item.hidden
            && !item.nonprinting
            && item.opacity > 0.0
            && d.layer(item.layer).is_some_and(|layer| layer.visible && layer.printable);
        for (index, _) in item.children().iter().enumerate() {
            if pending.len() >= 65536 {
                return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
            }
            let mut child = loc.clone();
            child.path.push(index);
            pending.push((child, placement, forced_page, visible));
        }
        if !visible {
            continue;
        }
        let Some(tf) = item.text_frame() else { continue };
        let story = d.stories.get(&tf.story).ok_or_else(|| bad(ID, format!("textLayoutStory: item {} has no story", item.id.0)))?;
        let composed = s.cache.get(d, tf.story, None);
        let nonempty = composed.frame(item.id).is_some_and(|frame| story.text.get(frame.range.clone()).is_some_and(|text| !text.trim().is_empty()));
        if !nonempty {
            continue;
        }
        let parent = placement * d.parent_xf(&loc);
        let bounds = parent.transform_rect_bbox(item.bounds());
        if ![bounds.x0, bounds.y0, bounds.x1, bounds.y1].iter().all(|v| v.is_finite()) {
            return Err(bad(ID, format!("textLayoutGeometry: item {} has nonfinite bounds", item.id.0)));
        }
        let page = if let Some(page) = forced_page {
            page
        } else {
            let SpreadRef::Doc(si) = loc.spread else { return Err(bad(ID, "textLayoutLocation: parent item without an owning page")) };
            d.first_page_of_spread(si)
                + d.spread(loc.spread)
                    .and_then(|sp| sp.page_at_x(bounds.center().x))
                    .ok_or_else(|| bad(ID, format!("textOutsideMargins: item {} has no owning page", item.id.0)))?
        };
        let owning = d.page(page).ok_or_else(|| bad(ID, "textLayoutLocation: no owning page"))?;
        frames.push(LayoutFrame { item, story, parent, bounds, safe: owning.margin_rect(), page, page_id: owning.id.0 });
        if frames.len() > 4096 {
            return Err(bad(ID, "textLayoutLimit: more than 4096 nonempty printable text-frame instances"));
        }
    }
    Ok(frames)
}

fn check_text_layout(original: &Session, draft: &Session) -> Result<()> {
    const TOLERANCE: f64 = 0.25;
    let before = layout_frames(original)?;
    let after = layout_frames(draft)?;
    let previous: std::collections::HashMap<_, _> = before.iter().map(|frame| ((frame.item.id, frame.page_id), frame)).collect();
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
    if !issues.is_empty() {
        return Err(bad(ID, format!("text layout rejected draft: {}", Value::Array(issues))));
    }
    Ok(())
}

fn check_image(p: &Value) -> Result<u64> {
    if p.get("frame").is_none() && !["x", "y", "width"].iter().all(|key| p.get(key).is_some()) {
        return Err(bad(ID, "image placement needs an explicit frame or x, y and width; loading the place cursor is not permitted"));
    }
    let encoded = p.get("base64").and_then(Value::as_str).ok_or_else(|| bad(ID, "inline image required"))?;
    let name = p.get("name").and_then(Value::as_str).ok_or_else(|| bad(ID, "image name required"))?.to_ascii_lowercase();
    let bytes = super::base64_decode(encoded);
    let png = bytes.starts_with(b"\x89PNG\r\n\x1a\n") && name.ends_with(".png");
    let jpeg = bytes.starts_with(&[0xff, 0xd8, 0xff]) && (name.ends_with(".jpg") || name.ends_with(".jpeg"));
    if !png && !jpeg {
        return Err(bad(ID, "only inline PNG/JPEG image bytes are permitted; SVG, PDF, paths and media are excluded"));
    }
    // Header inspection does not decode pixel data. Refuse PNG/JPEG decompression bombs first.
    let (w, h) = designcraft_images::pixel_size(&bytes).ok_or_else(|| bad(ID, "invalid raster image header"))?;
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > 16_000_000 {
        return Err(bad(ID, "image dimensions exceed 16 million pixels"));
    }
    Ok(u64::from(w) * u64::from(h))
}

/// Validate and simulate a complete production against the current document without editing it.
pub fn validate_actions(s: &Session, p: &Value) -> Result<Value> {
    let (_, results) = draft(s, p, || false)?;
    Ok(json!({"valid":true,"steps":results.len(),"results":results,"documentUid":s.doc()?.uid,"revision":s.doc()?.revision}))
}

/// Atomic publication, with cooperative cancellation checked between actions and before commit.
/// This public helper also records exactly one undo entry when called outside the registry.
pub fn apply_actions(s: &mut Session, p: &Value, cancelled: impl FnMut() -> bool) -> Result<Value> {
    let (mut finished, results) = draft(s, p, cancelled)?;
    let original = s.doc()?;
    let label = p.get("label").and_then(Value::as_str).unwrap_or("AI Production");
    finished.history = original.history.clone();
    if !std::sync::Arc::ptr_eq(&finished.doc, &original.doc) {
        crate::record_undo(
            &mut finished,
            HistoryEntry { label: label.to_string(), doc: original.doc.clone(), selection: original.selection.clone() },
        );
    }
    *s.doc_mut()? = finished;
    s.typing_format = None;
    s.reset_find_scope();
    Ok(json!({"applied":true,"steps":results.len(),"results":results,"label":label,"documentUid":s.doc()?.uid,"revision":s.doc()?.revision}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "production.catalog","AI Production Commands",[],None,"{} → vetted commands with paramsSchema and examples",always,|_,_| Ok(catalog())),
        cmd!(query "production.validate","Validate AI Production",[],None,"{commands: [{command, params}], outputMode, allowedCommands?, label?, expectedDocumentUid?, expectedRevision?, requirePreflight?: bool (default false; refuse error-level issues before publication), minPpi?: 150 (1 to 2400)} → {valid, steps, results}",has_doc,|s,p| validate_actions(s,p)),
        cmd!(noundo "production.apply","Apply AI Production",[],None,"{commands: [{command, params}], outputMode, allowedCommands?, label?, expectedDocumentUid?, expectedRevision?, requirePreflight?: bool (default false; refuse error-level issues before publication), minPpi?: 150 (1 to 2400)} → {applied, steps, results}",has_doc,|s,p| apply_actions(s,p,|| false)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn session() -> Session {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s
    }
    fn plan(commands: Value) -> Value {
        json!({"outputMode":"native_only","commands":commands})
    }
    fn sample() -> Value {
        plan(json!([
            {"command":"frame.create","params":{"rect":[36,36,540,200],"content":"text","text":"نص الدرس","caret":false}},
            {"command":"selection.set","params":{"ids":["$0.id"]}},
            {"command":"type.char","params":{"attrs":{"size":16}}},
            {"command":"type.para","params":{"attrs":{"align":"right","direction":"rightToLeft"}}},
            {"command":"story.setDirection","params":{"story":"$0.story","direction":"rightToLeft"}}
        ]))
    }
    #[test]
    fn native_production_has_one_labeled_undo_and_validates_without_edits() {
        let mut s = session();
        let before = s.doc().unwrap().doc.clone();
        let journal = s.journal.len();
        assert_eq!(s.execute("production.validate", &sample()).unwrap()["valid"], true);
        assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
        assert_eq!(s.journal.len(), journal);
        let mut p = sample();
        p["label"] = json!("School lesson");
        let r = s.execute("production.apply", &p).unwrap();
        assert_eq!(r["steps"], 5);
        assert_eq!(s.doc().unwrap().history.undo.len(), 1);
        assert_eq!(s.doc().unwrap().history.undo[0].label, "School lesson");
        assert_eq!(s.doc().unwrap().doc.spreads[0].items.len(), 1);
        s.execute("edit.undo", &json!({})).unwrap();
        assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
        s.execute("edit.redo", &json!({})).unwrap();
        assert_eq!(s.doc().unwrap().doc.spreads[0].items.len(), 1);
    }
    #[test]
    fn capabilities_cannot_be_broadened_or_nested() {
        let mut s = session();
        let before = s.doc().unwrap().doc.clone();
        for command in [
            "file.open",
            "file.new",
            "file.print",
            "document.exportPdf",
            "library.open",
            "script.run",
            "production.apply",
            "edit.undo",
            "data.preview",
            "object.pasteSvg",
            "prefs.set",
        ] {
            let mut p = plan(json!([{"command":command,"params":{}}]));
            p["allowedCommands"] = json!([command]);
            assert!(s.execute("production.apply", &p).is_err(), "{command}");
            assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
        }
        let mut p = sample();
        p["allowedCommands"] = json!(["frame.create"]);
        assert!(s.execute("production.apply", &p).is_err());
        assert_eq!(s.doc().unwrap().history.undo.len(), 0);
    }
    #[test]
    fn ordinary_errors_resolved_types_and_cancellation_leave_every_live_state_pristine() {
        let mut s = session();
        s.execute("frame.create", &json!({"rect":[0,0,50,50]})).unwrap();
        s.execute("edit.undo", &json!({})).unwrap();
        let before = s.doc().unwrap().clone();
        let journal = s.journal.len();
        for second in [
            json!({"command":"object.fill","params":{"ids":["$0.id"],"swatch":"missing swatch"}}),
            json!({"command":"object.fill","params":{"ids":["$0"],"swatch":"[Black]"}}),
            json!({"command":"transform.move","params":{"ids":["$0.id"],"dx":"8","dy":0}}),
            json!({"command":"type.char","params":{"attrs":{"size":12,"unsupported":true}}}),
        ] {
            let p = plan(json!([{"command":"frame.create","params":{"rect":[0,0,50,50]}},second]));
            assert!(s.execute("production.apply", &p).is_err());
            assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
            assert_eq!(format!("{:?}", s.doc().unwrap().selection), format!("{:?}", before.selection));
            assert_eq!(s.doc().unwrap().history.redo.len(), before.history.redo.len());
            assert_eq!(s.doc().unwrap().revision, before.revision);
            assert_eq!(s.journal.len(), journal);
        }
        let mut checks = 0;
        assert!(
            apply_actions(&mut s, &sample(), || {
                checks += 1;
                checks >= 2
            })
            .is_err()
        );
        assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
        assert_eq!(s.doc().unwrap().history.redo.len(), 1);
        assert!(
            apply_actions(&mut s, &plan(json!([{"command":"frame.create","params":{"rect":[0,0,50,50]}}])), || {
                checks += 1;
                checks >= 4
            })
            .is_err()
        );
        assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    }
    #[test]
    fn native_only_rejects_pixels_and_image_mode_never_accepts_svg_or_paths() {
        let mut s = session();
        for params in [
            json!({"name":"image.png","base64":""}),
            json!({"name":"image.svg","base64":super::super::base64_encode(b"<svg><image href='x'/></svg>")}),
            json!({"path":"private.png","name":"image.png","base64":""}),
        ] {
            let mut p = plan(json!([{"command":"file.place","params":params}]));
            assert!(s.execute("production.apply", &p).is_err());
            p["outputMode"] = json!("native_with_images");
            assert!(s.execute("production.apply", &p).is_err());
        }
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let p = json!({"outputMode":"native_with_images","commands":[{"command":"file.place","params":{"name":"illustration.png","base64":png,"x":36,"y":36,"width":100}}]});
        assert!(s.execute("production.apply", &p).is_ok());
        assert_eq!(s.doc().unwrap().history.undo.len(), 1);
    }
    #[test]
    fn stale_documents_and_hostile_plan_bounds_are_rejected() {
        let mut s = session();
        let mut p = sample();
        p["expectedDocumentUid"] = json!(s.doc().unwrap().uid);
        p["expectedRevision"] = json!(s.doc().unwrap().revision + 1);
        assert!(s.execute("production.apply", &p).is_err());
        p["expectedRevision"] = json!(s.doc().unwrap().revision);
        assert!(s.execute("production.validate", &p).is_ok());
        let too_many = plan(Value::Array(vec![json!({"command":"selection.set","params":{"ids":[]}}); MAX_ACTIONS + 1]));
        assert!(s.execute("production.validate", &too_many).is_err());
        let negative = plan(json!([{"command":"frame.create","params":{"rect":[0,0,50,50],"spread":-1}}]));
        assert!(s.execute("production.apply", &negative).is_err());
        let mut nested = json!(0);
        for _ in 0..30 {
            nested = json!([nested]);
        }
        assert!(bounded(&nested).is_err());
        let c = s.execute("production.catalog", &json!({})).unwrap();
        assert!(c["commands"].as_array().unwrap().iter().all(|v| v["paramsSchema"]["additionalProperties"] == false));
    }

    #[test]
    fn preflight_rejects_overset_on_the_draft_preserving_live_document_and_history() {
        let mut s = session();
        s.execute("frame.create", &json!({"rect":[0,0,50,50]})).unwrap();
        s.execute("edit.undo", &json!({})).unwrap();
        let before = s.doc().unwrap().clone();
        let journal = s.journal.len();
        let p = json!({"outputMode":"native_only","requirePreflight":true,"minPpi":150,
            "commands":[{"command":"frame.create","params":{"rect":[36,36,40,40],"content":"text","text":"overset text ".repeat(100)}}]});
        let err = s.execute("production.apply", &p).unwrap_err();
        assert!(err.to_string().contains("overset"), "{err}");
        assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
        assert_eq!(s.doc().unwrap().history.undo.len(), before.history.undo.len());
        assert_eq!(s.doc().unwrap().history.redo.len(), before.history.redo.len());
        assert_eq!(s.doc().unwrap().revision, before.revision);
        assert_eq!(format!("{:?}", s.doc().unwrap().selection), format!("{:?}", before.selection));
        assert_eq!(s.journal.len(), journal);
        assert!(s.execute("production.validate", &p).is_err());
        let success = json!({"outputMode":"native_only","requirePreflight":true,"minPpi":300,
            "commands":[{"command":"frame.create","params":{"rect":[36,36,200,200],"content":"unassigned"}},
                {"command":"object.fill","params":{"ids":["$0.id"],"swatch":"[Paper]"}}]});
        s.execute("production.apply", &success).unwrap();
        assert_eq!(s.doc().unwrap().history.undo.len(), before.history.undo.len() + 1);
        assert!(s.doc().unwrap().history.redo.is_empty());
        s.execute("edit.undo", &json!({})).unwrap();
        assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    }

    #[test]
    fn preflight_options_are_strictly_typed_and_bounded() {
        let mut s = session();
        let before = s.doc().unwrap().doc.clone();
        for (key, value) in [
            ("requirePreflight", json!("true")),
            ("requirePreflight", Value::Null),
            ("minPpi", json!(0)),
            ("minPpi", json!(2401)),
            ("minPpi", json!("150")),
        ] {
            let mut p = sample();
            p[key] = value;
            assert!(s.execute("production.apply", &p).is_err());
            assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
        }
    }

    fn layout_font() -> &'static str {
        static LOADED: std::sync::Once = std::sync::Once::new();
        LOADED.call_once(|| {
            designcraft_fonts::FontDb::global().add_font(designcraft_fonts::testing::font_with("DC Production Layout Guard", &['A', 'B']).unwrap());
        });
        "DC Production Layout Guard"
    }

    fn layout_text(s: &mut Session, rect: [f64; 4]) -> Value {
        let item = s.execute("frame.create", &json!({"rect":rect,"content":"text","text":"A","caret":false})).unwrap();
        s.execute("type.char", &json!({"attrs":{"fontFamily":layout_font(),"size":12}})).unwrap();
        item
    }

    fn layout_plan(rectangles: &[[f64; 4]]) -> Value {
        let mut commands = Vec::new();
        for rect in rectangles {
            commands.push(json!({"command":"frame.create","params":{"rect":rect,"content":"text","text":"A","caret":false}}));
            commands.push(json!({"command":"type.char","params":{"attrs":{"fontFamily":layout_font(),"size":12}}}));
        }
        json!({"outputMode":"native_only","requirePreflight":true,"commands":commands})
    }

    #[test]
    fn gated_text_overlap_and_unsafe_margins_leave_no_live_changes() {
        let mut s = session();
        let before = s.doc().unwrap().clone();
        for (rects, kind) in [
            (vec![[72.0, 72.0, 250.0, 150.0], [180.0, 100.0, 300.0, 180.0]], "textOverlap"),
            (vec![[20.0, 72.0, 150.0, 120.0]], "textOutsideMargins"),
        ] {
            let err = s.execute("production.apply", &layout_plan(&rects)).unwrap_err();
            assert!(err.to_string().contains(kind), "{err}");
            assert!(err.to_string().contains("\"item\"") && err.to_string().contains("\"page\":1"), "{err}");
            assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
            assert_eq!(s.doc().unwrap().history.undo.len(), before.history.undo.len());
            assert_eq!(s.doc().unwrap().revision, before.revision);
        }
        s.execute("production.apply", &layout_plan(&[[72.0, 72.0, 250.0, 120.0], [72.0, 150.0, 250.0, 198.0]])).unwrap();
        assert_eq!(s.doc().unwrap().history.undo.len(), 1);
        s.execute("edit.undo", &json!({})).unwrap();
        assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    }

    #[test]
    fn unchanged_old_overlap_allows_unrelated_work_but_story_changes_are_checked() {
        let mut s = session();
        let first = layout_text(&mut s, [72.0, 72.0, 250.0, 150.0]);
        layout_text(&mut s, [180.0, 100.0, 300.0, 180.0]);
        let unrelated = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
            {"command":"line.create","params":{"a":[72,300],"b":[400,300]}}]});
        s.execute("production.apply", &unrelated).unwrap();
        let before = s.doc().unwrap().doc.clone();
        let edits = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
            {"command":"story.setText","params":{"story":first["story"],"text":"B"}}]});
        let err = s.execute("production.apply", &edits).unwrap_err();
        assert!(err.to_string().contains("textOverlap"), "{err}");
        assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
    }

    #[test]
    fn group_parent_transforms_are_applied_to_text_layout_bounds() {
        let mut s = session();
        let a = layout_text(&mut s, [72.0, 72.0, 200.0, 140.0]);
        let shape = s.execute("frame.create", &json!({"rect":[72,72,80,80]})).unwrap();
        let group = s.execute("object.group", &json!({"ids":[a["id"],shape["id"]]})).unwrap();
        layout_text(&mut s, [300.0, 72.0, 450.0, 140.0]);
        let before = s.doc().unwrap().doc.clone();
        let move_group = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
            {"command":"transform.move","params":{"ids":[group["id"]],"dx":230,"dy":0}}]});
        let err = s.execute("production.apply", &move_group).unwrap_err();
        assert!(err.to_string().contains("textOverlap"), "{err}");
        assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
    }
}
