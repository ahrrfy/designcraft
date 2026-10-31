//! Atomic private-draft validation and publication for `production.apply` and `production.applyGroup`.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use super::super::bad;
use super::image_guard::check_image;
use super::layout_guard::check_text_layout;
use super::schema::{check, check_command_swatches, schemas};
use super::{
    GROUP_ID, ID, MAX_ACTIONS, MAX_ALLOWED_COMMANDS, MAX_BYTES, MAX_ENCODED_IMAGE_BYTES, MAX_GROUP_BYTES, MAX_GROUPS, MAX_UNIQUE_IMAGE_BYTES,
    MAX_UNIQUE_IMAGE_PIXELS,
};
use crate::{HistoryEntry, Result, Session};

fn item_contains_graphic(item: &designcraft_doc::Item) -> bool {
    let mut stack = vec![item];
    while let Some(current) = stack.pop() {
        if current.graphic().is_some() {
            return true;
        }
        stack.extend(current.children().iter().map(AsRef::as_ref));
    }
    false
}

// Iterative walk bounds recursion before calling the script reference resolver or cloning JSON.
pub(super) fn bounded(value: &Value) -> Result<()> {
    bounded_limits(value, ID, MAX_BYTES, 65536, 24)
}

fn bounded_limits(value: &Value, id: &str, max_bytes: usize, max_nodes: usize, max_depth: usize) -> Result<()> {
    let mut stack = vec![(value, 0usize)];
    let mut bytes = 0usize;
    let mut nodes = 0usize;
    while let Some((v, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if depth > max_depth || nodes > max_nodes {
            return Err(bad(id, "plan nesting or node limit exceeded"));
        }
        match v {
            Value::String(s) => bytes = bytes.saturating_add(s.len()),
            Value::Array(a) => {
                if a.len() > 65536 {
                    return Err(bad(id, "array too large"));
                }
                stack.extend(a.iter().map(|x| (x, depth + 1)));
            }
            Value::Object(o) => {
                if o.len() > 128 {
                    return Err(bad(id, "object too large"));
                }
                for (key, x) in o {
                    bytes = bytes.saturating_add(key.len());
                    stack.push((x, depth + 1));
                }
            }
            _ => {}
        }
        if bytes > max_bytes {
            return Err(bad(id, "plan byte limit exceeded"));
        }
    }
    Ok(())
}

fn deserialize_present<'de, T, D>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    <T as serde::Deserialize>::deserialize(deserializer).map(Some)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActionEnvelope {
    command: String,
    params: serde_json::Map<String, Value>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanEnvelope {
    commands: Vec<ActionEnvelope>,
    output_mode: String,
    #[serde(default, deserialize_with = "deserialize_present")]
    allowed_commands: Option<Vec<String>>,
    #[serde(default, deserialize_with = "deserialize_present")]
    label: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    expected_document_uid: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    expected_revision: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    require_preflight: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_present")]
    min_ppi: Option<f64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DocumentSetupEnvelope {
    #[serde(default, deserialize_with = "deserialize_present")]
    facing_pages: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_present")]
    binding: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ChunkEnvelope {
    #[allow(dead_code)]
    commands: Vec<ActionEnvelope>,
    #[serde(default, deserialize_with = "deserialize_present")]
    #[allow(dead_code)]
    label: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GroupEnvelope {
    chunks: Vec<ChunkEnvelope>,
    #[allow(dead_code)]
    output_mode: String,
    #[serde(default, deserialize_with = "deserialize_present")]
    label: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    expected_document_uid: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    expected_revision: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    #[allow(dead_code)]
    require_preflight: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_present")]
    #[allow(dead_code)]
    min_ppi: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    document_setup: Option<DocumentSetupEnvelope>,
}

fn draft(s: &Session, p: &Value, mut cancelled: impl FnMut() -> bool) -> Result<(crate::DocState, Vec<Value>)> {
    bounded(p)?;
    if !p.is_object() {
        return Err(bad(ID, "plan must be a JSON object"));
    }
    let env: PlanEnvelope = serde_path_to_error::deserialize(p).map_err(|e| bad(ID, e.to_string()))?;
    let require_preflight = env.require_preflight.unwrap_or(false);
    let min_ppi = match env.min_ppi {
        None => 150.0,
        Some(n) if n.is_finite() && (1.0..=2400.0).contains(&n) => n,
        Some(_) => return Err(bad(ID, "minPpi must be a finite number from 1 to 2400")),
    };
    let images = match env.output_mode.as_str() {
        "native_only" | "nativeOnly" => false,
        "native_with_images" | "nativeWithImages" => true,
        _ => return Err(bad(ID, "invalid outputMode")),
    };
    if env.commands.is_empty() || env.commands.len() > MAX_ACTIONS {
        return Err(bad(ID, "commands must contain 1 to 512 actions"));
    }
    if let Some(allowed) = &env.allowed_commands
        && allowed.len() > MAX_ALLOWED_COMMANDS
    {
        return Err(bad(ID, "allowedCommands must be a string array"));
    }
    if let Some(label) = &env.label
        && (label.trim().is_empty() || label.len() > 256)
    {
        return Err(bad(ID, "label must contain 1 to 256 bytes"));
    }
    let original = s.doc()?;
    if original.interaction.is_some() || original.preview_stash.is_some() || s.tool.composing() {
        return Err(bad(ID, "finish the current interaction or preview first"));
    }
    for (key, given, actual) in
        [("expectedDocumentUid", env.expected_document_uid, original.uid), ("expectedRevision", env.expected_revision, original.revision)]
    {
        if let Some(v) = given
            && v != actual
        {
            return Err(bad(ID, format!("{key}: document changed while generation was pending")));
        }
    }
    let mut vetted = schemas();
    // Providers cannot author file.place. Keep their public catalog stable,
    // while validated host-generated inline assets receive the print allowance.
    let image_schema = vetted.iter_mut().find(|(id, _, _)| *id == "file.place").ok_or_else(|| bad(ID, "missing inline image schema"))?;
    let encoded_limit = image_schema
        .1
        .get_mut("properties")
        .and_then(|properties| properties.get_mut("base64"))
        .and_then(|base64| base64.get_mut("maxLength"))
        .ok_or_else(|| bad(ID, "missing inline image byte limit"))?;
    *encoded_limit = json!(MAX_ENCODED_IMAGE_BYTES);
    // Reject every command and malformed envelope before executing the first draft action.
    for (i, action) in env.commands.iter().enumerate() {
        let id = action.command.as_str();
        if !vetted.iter().any(|(c, _, _)| *c == id)
            || (!images && id == "file.place")
            || env.allowed_commands.as_ref().is_some_and(|a| !a.iter().any(|c| c == id))
        {
            return Err(bad(ID, format!("commands[{i}].command: command `{id}` is not permitted")));
        }
    }
    let mut work = Session::new();
    work.cache = s.cache.clone();
    work.prefs = s.prefs.clone();
    work.docs.push(original.clone());
    work.active = Some(0);
    // A draft needs no per-action history or journal retention.
    work.in_undo_step = true;
    let mut results = Vec::with_capacity(env.commands.len());
    let mut image_sources = BTreeSet::new();
    let mut placed_raster_frames = BTreeSet::new();
    let mut image_bytes = 0usize;
    let mut image_pixels = 0u64;
    let mut table_cells = 0u64;
    for (i, action) in env.commands.into_iter().enumerate() {
        if cancelled() {
            return Err(bad(ID, "production cancelled"));
        }
        let id = action.command.as_str();
        let raw = Value::Object(action.params);
        let mut resolved = crate::script::resolve(&raw, &results).map_err(|e| bad(ID, format!("commands[{i}].params (step {i}): {e}")))?;
        bounded(&resolved)?;
        let schema = vetted.iter().find(|(c, _, _)| *c == id).ok_or_else(|| bad(ID, "command not vetted"))?;
        check(&schema.1, &resolved, &format!("commands[{i}].params")).map_err(|e| match e {
            crate::EngineError::BadParams { cmd, msg } => crate::EngineError::BadParams { cmd, msg: format!("{msg} ({id})") },
            other => other,
        })?;
        check_command_swatches(&work.doc()?.doc, id, &resolved)?;
        if id == "object.gradient"
            && let Some(stops) = resolved.get_mut("stops").and_then(Value::as_array_mut)
        {
            for stop in stops {
                if let Some(st) = stop.as_object_mut() {
                    if !st.contains_key("location")
                        && let Some(pos) = st.get("pos").and_then(Value::as_f64)
                    {
                        st.insert("location".into(), json!(pos * 100.0));
                    }
                    if st.get("color").is_none() && st.get("swatch").is_none() {
                        return Err(bad(ID, "object.gradient.stops: missing `color` or `swatch`"));
                    }
                }
            }
        }
        if id == "object.gradientFeather"
            && let Some(obj) = resolved.as_object_mut()
        {
            let s_val = obj.get("start").and_then(Value::as_f64);
            let e_val = obj.get("end").and_then(Value::as_f64);
            let unit_opacities = (s_val.is_some() || e_val.is_some())
                && s_val.is_none_or(|v| (0.0..=1.0).contains(&v))
                && e_val.is_none_or(|v| (0.0..=1.0).contains(&v))
                && (s_val.unwrap_or(0.0) > 0.0 || e_val.unwrap_or(0.0) > 0.0);
            if unit_opacities {
                if let Some(sv) = s_val {
                    obj.insert("start".into(), json!(sv * 100.0));
                }
                if let Some(ev) = e_val {
                    obj.insert("end".into(), json!(ev * 100.0));
                }
            }
        }
        if id == "transform.move" && resolved.get("copy").and_then(Value::as_bool) == Some(true) {
            let targets = resolved.get("ids").and_then(Value::as_array).ok_or_else(|| bad(ID, "raster copy needs explicit targets"))?;
            let source = targets.first().and_then(Value::as_u64).filter(|_| targets.len() == 1);
            let current = work.doc()?;
            if !images
                || !source.is_some_and(|source| {
                    placed_raster_frames.contains(&source)
                        && current
                            .doc
                            .item(designcraft_doc::ItemId(source))
                            .is_some_and(|item| item.graphic().is_some() && item.children().is_empty())
                })
            {
                return Err(bad(ID, "production copy requires one validated raster leaf placed in this private draft"));
            }
        }
        if id == "edit.stepAndRepeat" {
            let current = work.doc()?;
            let target_ids: Vec<designcraft_doc::ItemId> = if let Some(arr) = resolved.get("ids").and_then(Value::as_array) {
                arr.iter().filter_map(Value::as_u64).map(designcraft_doc::ItemId).collect()
            } else {
                current.selection.items.clone()
            };
            if target_ids.iter().any(|&item_id| current.doc.item(item_id).is_some_and(item_contains_graphic)) {
                return Err(bad(ID, "edit.stepAndRepeat cannot duplicate graphic frames in production"));
            }
        }
        if matches!(id, "frame.create" | "object.qrCode" | "text.thread")
            && let Some(rect_arr) = resolved.get("rect").and_then(Value::as_array)
        {
            let coordinates: Vec<_> = rect_arr.iter().filter_map(Value::as_f64).collect();
            if coordinates.get(2).zip(coordinates.first()).is_none_or(|(right, left)| right <= left)
                || coordinates.get(3).zip(coordinates.get(1)).is_none_or(|(bottom, top)| bottom <= top)
            {
                return Err(bad(ID, "frame rectangle must have positive width and height"));
            }
        }
        if id == "file.place" {
            let raster = check_image(&resolved)?;
            if image_sources.insert(raster.sha256) {
                image_bytes = image_bytes
                    .checked_add(raster.bytes)
                    .filter(|bytes| *bytes <= MAX_UNIQUE_IMAGE_BYTES)
                    .ok_or_else(|| bad(ID, "plan exceeds 32 MiB of unique raster bytes"))?;
                image_pixels = image_pixels
                    .checked_add(raster.pixels)
                    .filter(|pixels| *pixels <= MAX_UNIQUE_IMAGE_PIXELS)
                    .ok_or_else(|| bad(ID, "plan exceeds 40 million unique image pixels"))?;
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
        if matches!(id, "table.insertRow" | "table.insertColumn") {
            let count = resolved.get("count").and_then(Value::as_u64).unwrap_or(1);
            table_cells = table_cells.saturating_add(count.saturating_mul(32));
            if table_cells > 10000 {
                return Err(bad(ID, "plan exceeds 10000 table cells"));
            }
        }
        if matches!(
            id,
            "table.setCell"
                | "table.insertRow"
                | "table.insertColumn"
                | "table.deleteRow"
                | "table.deleteColumn"
                | "table.merge"
                | "table.unmerge"
                | "table.setRowHeight"
                | "table.setColumnWidth"
                | "table.distributeColumns"
                | "table.distributeRows"
                | "table.options"
                | "style.table.apply"
                | "style.cell.apply"
        ) && work.doc()?.selection.text.and_then(|t| t.cell).is_none()
            && let Some(table) = resolved.get("table").and_then(Value::as_u64)
        {
            let mut select_params = json!({"table": table});
            for key in ["story", "row", "rows", "col", "cols"] {
                if let Some(v) = resolved.get(key) {
                    select_params[key] = v.clone();
                }
            }
            work.execute("table.select", &select_params).map_err(|e| bad(ID, format!("step {i} ({id}): {e}")))?;
        }
        let result = work.execute(id, &resolved).map_err(|e| bad(ID, format!("step {i} ({id}): {e}")))?;
        if id == "file.place" {
            let frame = result.get("id").and_then(Value::as_u64).ok_or_else(|| bad(ID, "validated raster placement did not return a frame"))?;
            placed_raster_frames.insert(frame);
        }
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
        let issues = super::super::preflight::check(&work, min_ppi);
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

/// Validate and simulate a complete production against the current document without editing it.
pub fn validate_actions(s: &Session, p: &Value) -> Result<Value> {
    let (_, results) = draft(s, p, || false)?;
    Ok(json!({"valid":true,"steps":results.len(),"results":results,"documentUid":s.doc()?.uid,"revision":s.doc()?.revision}))
}

/// Atomic publication, with cooperative cancellation checked between actions and before commit.
/// This public helper also records exactly one undo entry when called outside the registry.
pub fn apply_actions(s: &mut Session, p: &Value, cancelled: impl FnMut() -> bool) -> Result<Value> {
    let (finished, results) = draft(s, p, cancelled)?;
    let label = p.get("label").and_then(Value::as_str).unwrap_or("AI Production");
    publish_draft(s, finished, label)?;
    Ok(json!({"applied":true,"steps":results.len(),"results":results,"label":label,"documentUid":s.doc()?.uid,"revision":s.doc()?.revision}))
}

fn publish_draft(s: &mut Session, mut finished: crate::DocState, label: &str) -> Result<()> {
    let original = s.doc()?;
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
    Ok(())
}

/// Publish a human review group atomically. Every chunk retains the single-plan bounds and
/// capabilities, including chunk-local `$N` references; no intermediate draft reaches `s`.
/// Cancellation or a later chunk's error leaves the live document and its history untouched.
pub fn apply_action_groups(s: &mut Session, p: &Value, mut cancelled: impl FnMut() -> bool) -> Result<Value> {
    // Two outer levels enclose the same per-plan shape: `chunks` and each chunk object.
    bounded_limits(p, GROUP_ID, MAX_GROUP_BYTES, MAX_GROUPS * 65536, 26)?;
    let root = p.as_object().ok_or_else(|| bad(GROUP_ID, "group plan must be a JSON object"))?;
    if let Some(raw_setup) = root.get("documentSetup")
        && !raw_setup.is_object()
    {
        return Err(bad(GROUP_ID, "documentSetup: expected an object"));
    }
    let env: GroupEnvelope = serde_path_to_error::deserialize(p).map_err(|e| bad(GROUP_ID, e.to_string()))?;
    if env.chunks.is_empty() || env.chunks.len() > MAX_GROUPS {
        return Err(bad(GROUP_ID, "chunks must contain 1 to 1000 plans"));
    }
    if let Some(label) = &env.label
        && (label.trim().is_empty() || label.len() > 256)
    {
        return Err(bad(GROUP_ID, "label must contain 1 to 256 bytes"));
    }
    // This host envelope is deliberately absent from the provider command schemas. A review
    // group may change pagination/binding atomically, but cannot smuggle size/page-count or
    // unrelated Document Setup changes through the host.
    let document_setup = match &env.document_setup {
        None => None,
        Some(setup) => {
            let mut map = serde_json::Map::new();
            if let Some(fp) = setup.facing_pages {
                map.insert("facingPages".into(), Value::Bool(fp));
            }
            if let Some(binding) = &setup.binding {
                if !matches!(binding.as_str(), "leftToRight" | "rightToLeft") {
                    return Err(bad(GROUP_ID, "documentSetup.binding must be leftToRight or rightToLeft"));
                }
                map.insert("binding".into(), Value::String(binding.clone()));
            }
            Some(Value::Object(map))
        }
    };
    let original = s.doc()?;
    if original.interaction.is_some() || original.preview_stash.is_some() || s.tool.composing() {
        return Err(bad(GROUP_ID, "finish the current interaction or preview first"));
    }
    for (key, given, actual) in
        [("expectedDocumentUid", env.expected_document_uid, original.uid), ("expectedRevision", env.expected_revision, original.revision)]
    {
        if let Some(v) = given
            && v != actual
        {
            return Err(bad(GROUP_ID, format!("{key}: document changed while generation was pending")));
        }
    }
    let original_page_count = original.doc.page_count();
    let mut work = Session::new();
    work.cache = s.cache.clone();
    work.prefs = s.prefs.clone();
    work.docs.push(original.clone());
    work.active = Some(0);
    work.in_undo_step = true;
    if let Some(setup) = &document_setup {
        if cancelled() {
            return Err(bad(GROUP_ID, "production cancelled"));
        }
        work.execute("layout.documentSetup", setup)?;
        if work.doc()?.doc.page_count() != original_page_count {
            return Err(bad(GROUP_ID, "documentSetup cannot add or remove existing document pages"));
        }
    }
    let require_preflight = env.require_preflight.unwrap_or(false);
    let min_ppi = match env.min_ppi {
        None => 150.0,
        Some(n) if n.is_finite() && (1.0..=2400.0).contains(&n) => n,
        Some(_) => return Err(bad(GROUP_ID, "minPpi must be a finite number from 1 to 2400")),
    };
    let chunks = p.get("chunks").and_then(Value::as_array).ok_or_else(|| bad(GROUP_ID, "chunks must contain 1 to 1000 plans"))?;
    let mut results = Vec::with_capacity(chunks.len());
    let mut steps = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        if cancelled() {
            return Err(bad(GROUP_ID, "production cancelled"));
        }
        bounded(chunk)?;
        let chunk = chunk.as_object().ok_or_else(|| bad(GROUP_ID, format!("chunks[{index}]: expected an object")))?;
        let mut chunk_params = serde_json::Map::new();
        for key in ["outputMode", "minPpi"] {
            if let Some(value) = p.get(key) {
                chunk_params.insert(key.into(), value.clone());
            }
        }
        chunk_params.extend(chunk.clone());
        // Replay intermediate chunks without rejecting transient errors that a later chunk repairs
        // (e.g. `font.replace` followed by `story.fitOverset`), then preflight the complete combined draft below.
        chunk_params.insert("requirePreflight".into(), Value::Bool(false));
        let (finished, chunk_results) = draft(&work, &Value::Object(chunk_params), &mut cancelled).map_err(|e| match e {
            crate::EngineError::BadParams { msg, .. } if msg.starts_with("commands[") => {
                crate::EngineError::BadParams { cmd: GROUP_ID.into(), msg: format!("chunks[{index}].{msg}") }
            }
            other => other,
        })?;
        *work.doc_mut()? = finished;
        steps = steps.saturating_add(chunk_results.len());
        results.push(chunk_results);
    }
    if require_preflight {
        check_text_layout(s, &work)?;
        let issues = super::super::preflight::check(&work, min_ppi);
        let errors: Vec<_> = issues.iter().filter(|issue| issue.severity == "error").collect();
        if !errors.is_empty() {
            let summary = errors.iter().take(8).map(|issue| format!("{}: {}", issue.kind, issue.message)).collect::<Vec<_>>().join("; ");
            return Err(bad(GROUP_ID, format!("preflight rejected draft ({} errors): {summary}", errors.len())));
        }
    }
    if cancelled() {
        return Err(bad(GROUP_ID, "production cancelled"));
    }
    let label = env.label.as_deref().unwrap_or("AI Production Group");
    publish_draft(s, work.doc()?.clone(), label)?;
    Ok(
        json!({"applied":true,"groups":results.len(),"steps":steps,"results":results,"label":label,"documentUid":s.doc()?.uid,"revision":s.doc()?.revision}),
    )
}
