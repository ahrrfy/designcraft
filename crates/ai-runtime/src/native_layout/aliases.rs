use super::{BTreeMap, BTreeSet, DocState, EngineAction, ProductionPlan, Value, json};

pub(super) fn normalize_effects_aliases(effects: &mut serde_json::Map<String, Value>) {
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

pub(super) fn normalize_alt_fills_aliases(alt: &mut serde_json::Map<String, Value>) {
    for (alias, canonical) in
        [("everyA", "first"), ("colorA", "firstColor"), ("tintA", "firstTint"), ("everyB", "next"), ("colorB", "nextColor"), ("tintB", "nextTint")]
    {
        if let Some(val) = alt.remove(alias) {
            alt.entry(canonical).or_insert(val);
        }
    }
}

fn parse_step_field(raw: &str, current_idx: usize) -> Option<(usize, &str)> {
    let body = raw.strip_prefix("${").and_then(|b| b.strip_suffix('}')).or_else(|| raw.strip_prefix('$'))?;
    let (step_str, field) = body.split_once('.')?;
    let resolved_step = if step_str == "last" { current_idx.checked_sub(1)? } else { step_str.parse::<usize>().ok()? };
    (resolved_step < current_idx).then_some((resolved_step, field))
}

fn is_text_frame_action(a: &EngineAction) -> bool {
    a.command == "frame.create"
        && (a.params.get("content").and_then(Value::as_str) == Some("text") || a.params.get("text").and_then(Value::as_str).is_some())
}

fn is_non_text_item_creation(a: &EngineAction) -> bool {
    (a.command == "frame.create" && !is_text_frame_action(a))
        || matches!(a.command.as_str(), "shape.create" | "line.create" | "path.create" | "object.qrCode")
}

fn canonical_alias_target(original: &str, has_style_name: bool) -> (Option<&'static str>, Option<&'static str>, Option<&'static str>) {
    match original {
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
    }
}

fn normalize_single_command_alias(index: usize, action: &mut EngineAction, notes: &mut Vec<Value>) {
    let original = action.command.trim();
    let has_style_name = action.params.get("name").is_some() || action.params.get("style").is_some();
    let (canonical, inferred_op, inferred_list_type) = canonical_alias_target(original, has_style_name);
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
    normalize_frame_and_shape_param_aliases(index, action, notes);
    normalize_style_and_layout_param_aliases(action);
}

fn normalize_frame_and_shape_param_aliases(index: usize, action: &mut EngineAction, notes: &mut Vec<Value>) {
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
}

fn normalize_style_and_layout_param_aliases(action: &mut EngineAction) {
    if matches!(
        action.command.as_str(),
        "style.cell.apply" | "style.table.apply" | "style.object.apply" | "style.paragraph.apply" | "style.character.apply"
    ) && let Some(obj) = action.params.as_object_mut()
        && !obj.contains_key("name")
        && let Some(style) = obj.remove("style")
    {
        obj.insert("name".into(), style);
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

fn collect_placed_frame_steps(plan: &ProductionPlan) -> BTreeSet<usize> {
    let mut placed_frame_steps = BTreeSet::new();
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
    placed_frame_steps
}

fn upgrade_downstream_story_frames(plan: &mut ProductionPlan, notes: &mut Vec<Value>) -> BTreeMap<usize, String> {
    let placed_frame_steps = collect_placed_frame_steps(plan);
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
    paired_story_for_non_text
}

struct AliasStepClasses {
    table_inserts: BTreeSet<usize>,
    id_array_producers: BTreeSet<usize>,
    null_formatting_steps: BTreeSet<usize>,
    non_text_creations: BTreeSet<usize>,
}

impl AliasStepClasses {
    fn collect(plan: &ProductionPlan) -> Self {
        let table_inserts = plan.commands.iter().enumerate().filter_map(|(i, a)| (a.command == "table.insert").then_some(i)).collect();
        let id_array_producers = plan
            .commands
            .iter()
            .enumerate()
            .filter_map(|(i, a)| matches!(a.command.as_str(), "transform.move" | "edit.stepAndRepeat" | "object.releaseCompoundPath").then_some(i))
            .collect();
        let null_formatting_steps =
            plan.commands.iter().enumerate().filter_map(|(i, a)| is_null_formatting_command(&a.command).then_some(i)).collect();
        let non_text_creations = plan.commands.iter().enumerate().filter_map(|(i, a)| is_non_text_item_creation(a).then_some(i)).collect();
        Self { table_inserts, id_array_producers, null_formatting_steps, non_text_creations }
    }
}

fn is_null_formatting_command(command: &str) -> bool {
    matches!(
        command,
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
}

#[derive(Default)]
struct AliasRefTracker {
    current_item_ref: Option<String>,
    current_story_ref: Option<String>,
    item_ref_to_story_ref: BTreeMap<String, String>,
    story_ref_to_item_ref: BTreeMap<String, String>,
    step_item_ref: BTreeMap<usize, String>,
    step_story_ref: BTreeMap<usize, String>,
    explicit_item_steps: BTreeSet<usize>,
}

impl AliasRefTracker {
    fn resolve_item_step(
        &self,
        source_idx: usize,
        current_idx: usize,
        prefer_non_text_next: bool,
        non_text_creations: &BTreeSet<usize>,
    ) -> Option<String> {
        if !self.explicit_item_steps.contains(&source_idx)
            && prefer_non_text_next
            && source_idx.saturating_add(1) < current_idx
            && non_text_creations.contains(&source_idx.saturating_add(1))
        {
            Some(format!("${}.id", source_idx.saturating_add(1)))
        } else {
            self.step_item_ref.get(&source_idx).cloned()
        }
    }
}

fn rewrite_scalar_step_references(
    index: usize,
    action: &mut EngineAction,
    classes: &AliasStepClasses,
    paired_story_for_non_text: &BTreeMap<usize, String>,
    refs: &AliasRefTracker,
    notes: &mut Vec<Value>,
) {
    let prefer_non_text_next = matches!(action.command.as_str(), "object.stroke" | "object.fill" | "object.fittingOptions");
    if let Some(table_val) = action.params.get_mut("table")
        && let Some(raw_ref) = table_val.as_str()
        && let Some((source_idx, "id")) = parse_step_field(raw_ref, index)
        && classes.table_inserts.contains(&source_idx)
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
        } else if classes.null_formatting_steps.contains(&source_idx)
            && let Some(resolved) = refs.step_story_ref.get(&source_idx).cloned()
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
            && classes.null_formatting_steps.contains(&source_idx)
            && let Some(resolved) = refs.resolve_item_step(source_idx, index, prefer_non_text_next, &classes.non_text_creations)
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
}

fn rewrite_null_formatting_ids(
    index: usize,
    prefer_non_text_next: bool,
    ids_val: &mut Value,
    classes: &AliasStepClasses,
    refs: &AliasRefTracker,
    notes: &mut Vec<Value>,
) {
    match ids_val {
        Value::Array(items) => {
            for elem in items.iter_mut() {
                if let Some(raw_ref) = elem.as_str()
                    && let Some((source_idx, "id")) = parse_step_field(raw_ref, index)
                    && classes.null_formatting_steps.contains(&source_idx)
                    && let Some(resolved) = refs.resolve_item_step(source_idx, index, prefer_non_text_next, &classes.non_text_creations)
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
                && classes.null_formatting_steps.contains(&source_idx)
                && let Some(resolved) = refs.resolve_item_step(source_idx, index, prefer_non_text_next, &classes.non_text_creations)
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
}

fn unwrap_or_wrap_ids_param(index: usize, ids_val: &mut Value, id_array_producers: &BTreeSet<usize>, notes: &mut Vec<Value>) {
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
                        if resolved_step.is_some_and(|s| s < index && id_array_producers.contains(&s)) {
                            let resolved = if raw.starts_with("${") { format!("${{{step_str}.ids}}") } else { format!("${step_str}.ids") };
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
                        replacement = Some((raw.clone(), Value::String(resolved), "Normalized array-producing command reference from .id to .ids"));
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

fn invalidate_cleared_refs(index: usize, action: &EngineAction, refs: &mut AliasRefTracker) {
    let mut invalidated_items = Vec::new();
    if let Some(arr) = action.params.get("ids").and_then(Value::as_array) {
        for raw in arr.iter().filter_map(Value::as_str) {
            if let Some((src, "id")) = parse_step_field(raw, index) {
                invalidated_items.push(format!("${src}.id"));
            } else if raw.starts_with('$') {
                invalidated_items.push(raw.to_owned());
            }
        }
    } else if let Some(cur) = refs.current_item_ref.take() {
        invalidated_items.push(cur);
    }
    refs.current_item_ref = None;
    refs.current_story_ref = None;
    for dead_item in invalidated_items {
        let dead_story = refs.item_ref_to_story_ref.remove(&dead_item);
        if let Some(ds) = &dead_story {
            refs.story_ref_to_item_ref.remove(ds);
            refs.step_story_ref.retain(|_, v| v != ds);
        }
        refs.step_item_ref.retain(|_, v| v != &dead_item);
    }
}

fn track_action_producer_refs(index: usize, action: &EngineAction, refs: &mut AliasRefTracker) {
    match action.command.as_str() {
        "frame.create" => {
            let item_r = format!("${index}.id");
            refs.current_item_ref = Some(item_r.clone());
            if is_text_frame_action(action) {
                let story_r = format!("${index}.story");
                refs.item_ref_to_story_ref.insert(item_r.clone(), story_r.clone());
                refs.story_ref_to_item_ref.insert(story_r.clone(), item_r);
                refs.current_story_ref = Some(story_r);
            } else {
                refs.current_story_ref = None;
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
            refs.current_item_ref = Some(format!("${index}.id"));
            refs.current_story_ref = None;
        }
        "edit.stepAndRepeat" | "object.releaseCompoundPath" | "selection.clear" => {
            refs.current_item_ref = None;
            refs.current_story_ref = None;
        }
        "edit.clear" | "object.ungroup" => invalidate_cleared_refs(index, action, refs),
        "selection.set" => {
            if let Some(arr) = action.params.get("ids").and_then(Value::as_array)
                && arr.len() == 1
                && let Some(id_str) = arr.first().and_then(Value::as_str)
                && id_str.starts_with('$')
            {
                refs.current_item_ref = Some(id_str.to_owned());
                refs.step_item_ref.insert(index, id_str.to_owned());
                refs.explicit_item_steps.insert(index);
                refs.current_story_ref = refs.item_ref_to_story_ref.get(id_str).cloned();
                if let Some(sr) = &refs.current_story_ref {
                    refs.step_story_ref.insert(index, sr.clone());
                }
            } else {
                refs.current_item_ref = None;
                refs.current_story_ref = None;
            }
        }
        "text.select" | "story.setText" | "story.setDirection" => {
            if let Some(sr) = action.params.get("story").and_then(Value::as_str)
                && sr.starts_with('$')
            {
                refs.current_story_ref = Some(sr.to_owned());
                refs.step_story_ref.insert(index, sr.to_owned());
                if let Some(ir) = refs.story_ref_to_item_ref.get(sr).cloned() {
                    refs.current_item_ref = Some(ir.clone());
                    refs.step_item_ref.insert(index, ir);
                    refs.explicit_item_steps.insert(index);
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
        | "style.cell.apply" => track_story_formatting_refs(index, action, refs),
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
        | "object.altText"
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
        | "style.object.apply" => track_item_formatting_refs(index, action, refs),
        _ => {}
    }
}

fn track_story_formatting_refs(index: usize, action: &EngineAction, refs: &mut AliasRefTracker) {
    if let Some(sr) = action.params.get("story").and_then(Value::as_str)
        && sr.starts_with('$')
    {
        refs.step_story_ref.insert(index, sr.to_owned());
        if let Some(ir) = refs.story_ref_to_item_ref.get(sr).cloned() {
            refs.step_item_ref.insert(index, ir);
            refs.explicit_item_steps.insert(index);
        }
    } else if matches!(action.command.as_str(), "type.char" | "type.para" | "style.paragraph.apply" | "style.character.apply" | "text.insert") {
        if let Some(ir) = &refs.current_item_ref {
            refs.step_item_ref.insert(index, ir.clone());
        }
        if let Some(sr) = &refs.current_story_ref {
            refs.step_story_ref.insert(index, sr.clone());
        }
    }
}

fn track_item_formatting_refs(index: usize, action: &EngineAction, refs: &mut AliasRefTracker) {
    if let Some(arr) = action.params.get("ids").and_then(Value::as_array)
        && arr.len() == 1
        && let Some(id_str) = arr.first().and_then(Value::as_str)
        && id_str.starts_with('$')
    {
        refs.step_item_ref.insert(index, id_str.to_owned());
        refs.explicit_item_steps.insert(index);
        if let Some(sr) = refs.item_ref_to_story_ref.get(id_str) {
            refs.step_story_ref.insert(index, sr.clone());
        }
    } else if action.params.get("ids").is_none() {
        if let Some(ir) = &refs.current_item_ref {
            refs.step_item_ref.insert(index, ir.clone());
        }
        if let Some(sr) = &refs.current_story_ref {
            refs.step_story_ref.insert(index, sr.clone());
        }
    }
}

/// Normalize common model-emitted command aliases (such as `type.set` -> `story.setText`,
/// `style.para.*` -> `style.paragraph.*`, `style.char.*` -> `style.character.*`,
/// `pathfinder.*` -> `object.pathfinder`, `qr.create` -> `object.qrCode`) before validation.
pub(crate) fn normalize_command_aliases(plan: &mut ProductionPlan) -> Vec<Value> {
    let mut notes = Vec::new();
    for (index, action) in plan.commands.iter_mut().enumerate() {
        normalize_single_command_alias(index, action, &mut notes);
    }
    let paired_story_for_non_text = upgrade_downstream_story_frames(plan, &mut notes);
    let classes = AliasStepClasses::collect(plan);
    let mut refs = AliasRefTracker::default();
    for (index, action) in plan.commands.iter_mut().enumerate() {
        rewrite_scalar_step_references(index, action, &classes, &paired_story_for_non_text, &refs, &mut notes);
        let prefer_non_text_next = matches!(action.command.as_str(), "object.stroke" | "object.fill" | "object.fittingOptions");
        if let Some(ids_val) = action.params.get_mut("ids") {
            rewrite_null_formatting_ids(index, prefer_non_text_next, ids_val, &classes, &refs, &mut notes);
            unwrap_or_wrap_ids_param(index, ids_val, &classes.id_array_producers, &mut notes);
        }
        track_action_producer_refs(index, action, &mut refs);
    }
    notes
}

/// A generation plan has already supplied its complete copy. A start caret
/// followed by character formatting would otherwise style only future typing,
/// leaving the generated words at the engine defaults. Recover this narrow
/// no-op using a real UTF-8 selection, never changing general caret semantics.
pub(super) fn format_populated_new_stories(plan: &mut ProductionPlan) -> Result<Vec<Value>, String> {
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

pub(super) fn normalize_unit_interval_fields(plan: &mut ProductionPlan) -> Vec<Value> {
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

pub(super) fn parse_keyed_channels<const N: usize>(raw: &str, keys: [char; N], max: f32) -> Option<[f32; N]> {
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

pub(super) fn parse_inline_color(raw: &str) -> Option<designcraft_engine::doc::color::Color> {
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
