//! Private page projections for the page-isolated AI planner. Published documents retain the
//! engine's real facing-page spreads; only the worker sees one spread per absolute page.
use designcraft_ai_types::protocol::ProductionPlan;
use designcraft_engine::{DocState, Prefs, Session, doc};
use serde_json::{Value, json};
use std::sync::Arc;

const MAX_PAGES: usize = 9999;
const MAX_COORD: f64 = 15552.0;

/// A read-only projection. Page/item identifiers and page-local artwork remain unchanged.
/// The engine allocates new SpreadIds: callers comparing two independently projected states
/// must match spreads by PageId, rather than treating SpreadId allocation as a document edit.
/// UID/revision are intentionally the live document pins, never a publication of this draft.
pub(crate) fn canonicalize(source: &DocState, prefs: &Prefs) -> Result<DocState, String> {
    validate_pages(source)?;
    validate_item_trees(source)?;
    if !source.doc.settings.facing_pages && source.doc.spreads.iter().all(|spread| spread.pages.len() == 1) {
        return Ok(source.clone());
    }
    let original_pages: Vec<_> = source.doc.pages().map(|(_, _, page)| page.clone()).collect();
    let mut result = setup(source, prefs, false, source.doc.settings.right_to_left_binding)?;
    // Single pages still display their original parent-page side. Otherwise turning an RTL
    // pair into singles would select the wrong parent artwork and asymmetric margins.
    let document = Arc::make_mut(&mut result.doc);
    for (spread, original) in document.spreads.iter_mut().zip(&original_pages) {
        let page = Arc::make_mut(spread).pages.first_mut().ok_or("Canonical page disappeared")?;
        if page.id != original.id {
            return Err("Canonical projection changed absolute page order".into());
        }
        page.side = original.side;
        page.margins = original.margins;
        page.columns = original.columns.clone();
        page.local_margins = original.local_margins;
    }
    Ok(result)
}

/// Make a real facing-page proof through Document Setup, including RTL page/binding order.
/// This is suitable for the saved native proof and spread previews. For individual PNGs use
/// render_page's absolute page API, or canonicalize this private proof before rendering.
pub(crate) fn formatted_proof_state(source: &DocState, prefs: &Prefs, right_to_left: bool) -> Result<DocState, String> {
    validate_pages(source)?;
    validate_item_trees(source)?;
    if source.doc.settings.facing_pages && source.doc.settings.right_to_left_binding == right_to_left {
        return Ok(source.clone());
    }
    setup(source, prefs, true, right_to_left)
}

fn validate_pages(source: &DocState) -> Result<(), String> {
    let count = source.doc.spreads.iter().try_fold(0usize, |count, spread| count.checked_add(spread.pages.len()).filter(|count| *count <= MAX_PAGES));
    let Some(count) = count.filter(|count| *count > 0) else {
        return Err("Facing-page projection requires 1 to 9999 pages".into());
    };
    if source.doc.spreads.iter().any(|spread| spread.pages.is_empty()) {
        return Err("Facing-page projection cannot discard an empty spread".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    for (_, _, page) in source.doc.pages() {
        if !ids.insert(page.id) || !page.x.is_finite() || ![page.width, page.height].iter().all(|n| n.is_finite() && *n > 0.0 && *n <= MAX_COORD) {
            return Err("Facing-page projection has duplicate page IDs or invalid geometry".into());
        }
    }
    let reserve = u64::try_from(count).map_err(|_| "Invalid projection page count")?.checked_add(1).ok_or("Projection ID reserve overflow")?;
    source.doc.next_id.checked_add(reserve).ok_or("Insufficient document identifiers for facing-page projection")?;
    Ok(())
}

fn setup(source: &DocState, prefs: &Prefs, facing: bool, rtl: bool) -> Result<DocState, String> {
    let mut private = source.clone();
    // repaginate deliberately retains non-shuffling pairs. The projection needs all pages to
    // participate; this changes only the private copy, never the live spread shuffle policy.
    for spread in &mut Arc::make_mut(&mut private.doc).spreads {
        Arc::make_mut(spread).allow_shuffle = true;
    }
    let mut scratch = Session::new();
    scratch.prefs = prefs.clone();
    scratch.add_document(private);
    scratch
        .execute("layout.documentSetup", &json!({"facingPages":facing,"binding":if rtl {"rightToLeft"} else {"leftToRight"}}))
        .map_err(|error| error.to_string())?;
    let mut result = source.clone();
    result.doc = scratch.doc().map_err(|error| error.to_string())?.doc.clone();
    if result.doc.page_count() != source.doc.page_count() || (!facing && result.doc.spreads.iter().any(|spread| spread.pages.len() != 1)) {
        return Err("Document Setup failed to preserve the projected absolute pages".into());
    }
    Ok(result)
}

/// Convert the planner's absolute-page spread indices into the live document's actual
/// spread indices. Result references and command count/order remain byte-for-byte intact.
/// Illustrations must already be expanded into native frame/file.place commands.
pub(crate) fn map_plan(plan: &ProductionPlan, actual: &DocState) -> Result<ProductionPlan, String> {
    validate_pages(actual)?;
    if plan.commands.len() > 512 {
        return Err("Facing-page plan exceeds the native action limit".into());
    }
    if !plan.images.is_empty() {
        return Err("Expand illustration placements before mapping facing pages".into());
    }
    let mut mapped = plan.clone();
    let mut result_pages: Vec<Option<usize>> = Vec::with_capacity(plan.commands.len());
    let mut selected: Vec<usize> = Vec::new();
    for (index, action) in mapped.commands.iter_mut().enumerate() {
        let params = action.params.as_object_mut().ok_or("Facing-page command parameters must be an object")?;
        let creation = matches!(action.command.as_str(), "frame.create" | "line.create" | "path.create");
        let standalone_image = action.command == "file.place" && !params.contains_key("frame");
        let mut result_page = None;
        if creation || standalone_image {
            let absolute = params.get("spread").and_then(Value::as_u64).ok_or("Every projected object requires an absolute page spread")?;
            let absolute = usize::try_from(absolute).map_err(|_| "Projected page index is too large")?;
            let (spread, page) = page_location(actual, absolute)?;
            params.insert("spread".into(), json!(spread));
            if let Some(rect) = params.get_mut("rect") {
                translate_array(rect, 4, &[0, 2], page.x)?;
            }
            for key in ["a", "b"] {
                if let Some(point) = params.get_mut(key) {
                    translate_array(point, 2, &[0], page.x)?;
                }
            }
            if let Some(anchors) = params.get_mut("anchors") {
                let anchors = anchors.as_array_mut().ok_or("Projected path anchors must be an array")?;
                if anchors.len() > 1024 {
                    return Err("Projected path exceeds the anchor limit".into());
                }
                for anchor in anchors {
                    let anchor = anchor.as_object_mut().ok_or("Projected path anchor must be an object")?;
                    for key in ["p", "in", "out"] {
                        if let Some(point) = anchor.get_mut(key) {
                            translate_array(point, 2, &[0], page.x)?;
                        }
                    }
                }
            }
            if standalone_image {
                let x = params.entry("x").or_insert(json!(0));
                translate_number(x, page.x)?;
            }
            selected = vec![absolute];
            result_page = Some(absolute);
        } else if action.command == "selection.set" {
            let pages = target_pages(params.get("ids"), &result_pages, index, actual)?;
            if params.get("add").and_then(Value::as_bool) != Some(true) {
                selected.clear();
            }
            for page in pages {
                if !selected.contains(&page) {
                    selected.push(page);
                }
            }
        } else if action.command == "object.group" || action.command == "transform.set" {
            let pages = match params.get("ids") {
                Some(ids) => target_pages(Some(ids), &result_pages, index, actual)?,
                None => selected.clone(),
            };
            if action.command == "object.group" {
                if pages.len() != 1 {
                    return Err("Projected groups must remain on one absolute page".into());
                }
                selected = pages.clone();
                result_page = pages.first().copied();
            } else if params.contains_key("x") {
                let absolute = *pages.first().ok_or("Absolute transform has no projected target page")?;
                if pages.iter().any(|page| *page != absolute) {
                    return Err("Absolute transforms cannot combine different projected pages".into());
                }
                let (spread, page) = page_location(actual, absolute)?;
                // transform.set x is measured from the document ruler origin. Page rulers
                // already include the page offset; spread and spine rulers need compensation.
                let base =
                    actual.doc.ruler_base(doc::SpreadRef::Doc(spread), page.x + page.width / 2.0).ok_or("Transform ruler origin is missing")?;
                let canonical_base = match actual.doc.settings.ruler_origin {
                    doc::RulerOrigin::Spine => match page.side {
                        doc::PageSide::Left => page.width,
                        doc::PageSide::Right => 0.0,
                        doc::PageSide::Single => page.width / 2.0,
                    },
                    _ => 0.0,
                };
                if let Some(x) = params.get_mut("x") {
                    translate_number(x, page.x + canonical_base - base.x)?;
                }
            }
        }
        result_pages.push(result_page);
    }
    Ok(mapped)
}

fn page_location(actual: &DocState, absolute: usize) -> Result<(usize, &doc::Page), String> {
    let (spread, page) = actual.doc.page_loc(absolute).ok_or("Projected object targets a missing absolute page")?;
    let page = actual.doc.spreads.get(spread).and_then(|spread| spread.pages.get(page)).ok_or("Projected page disappeared")?;
    Ok((spread, page))
}

fn target_pages(ids: Option<&Value>, results: &[Option<usize>], current: usize, actual: &DocState) -> Result<Vec<usize>, String> {
    let ids = ids.and_then(Value::as_array).ok_or("Projected targets must be an ID array")?;
    if ids.len() > 1024 {
        return Err("Projected target count exceeds its limit".into());
    }
    let mut pages = Vec::new();
    for id in ids {
        let page = if let Some(reference) = id.as_str() {
            let reference = reference.strip_prefix("${").and_then(|text| text.strip_suffix('}')).or_else(|| reference.strip_prefix('$'));
            let (step, field) = reference.and_then(|text| text.split_once('.')).ok_or("Invalid projected result reference")?;
            let step = step.parse::<usize>().map_err(|_| "Invalid projected result index")?;
            if step >= current || field != "id" {
                return Err("Projected object targets must reference an earlier object ID".into());
            }
            results.get(step).copied().flatten().ok_or("Projected reference has no target page")?
        } else if let Some(id) = id.as_u64() {
            let location = actual.doc.find(doc::ItemId(id)).ok_or("Projected literal target does not exist")?;
            let doc::SpreadRef::Doc(spread) = location.spread else {
                return Err("Projected transforms cannot target parent items".into());
            };
            let spread_ref = actual.doc.spreads.get(spread).ok_or("Projected target spread disappeared")?;
            let item = actual.doc.item(doc::ItemId(id)).ok_or("Projected target disappeared")?;
            let page = spread_ref.page_at_x(item.bounds().center().x).ok_or("Projected target page disappeared")?;
            actual.doc.first_page_of_spread(spread).checked_add(page).ok_or("Projected target page overflow")?
        } else {
            return Err("Projected targets require object IDs or result references".into());
        };
        if !pages.contains(&page) {
            pages.push(page);
        }
    }
    Ok(pages)
}

fn translate_array(value: &mut Value, count: usize, indices: &[usize], dx: f64) -> Result<(), String> {
    let values = value.as_array_mut().filter(|values| values.len() == count).ok_or("Invalid projected coordinate array")?;
    if values.iter().any(|value| !value.as_f64().is_some_and(f64::is_finite)) {
        return Err("Projected coordinates must be finite numbers".into());
    }
    for index in indices {
        let value = values.get_mut(*index).ok_or("Invalid projected coordinate index")?;
        translate_number(value, dx)?;
    }
    Ok(())
}

fn translate_number(value: &mut Value, dx: f64) -> Result<(), String> {
    let number = value.as_f64().filter(|number| number.is_finite()).ok_or("Projected coordinate must be finite")? + dx;
    if !number.is_finite() || number.abs() > MAX_COORD {
        return Err("Mapped facing-page coordinate exceeds the native command limit".into());
    }
    *value = json!(number);
    Ok(())
}

/// Validate publication directly in the actual spread topology. The two documents must be
/// the actual pre/post execution states, never independently repaginated projections.
pub(crate) fn validate_mapped_result(before: &DocState, after: &DocState, scope: crate::worker::BatchScope) -> Result<(), String> {
    validate_pages(before)?;
    validate_pages(after)?;
    let start = usize::try_from(scope.start).map_err(|_| "Invalid facing batch start")?;
    let end = start
        .checked_add(usize::try_from(scope.count).map_err(|_| "Invalid facing batch count")?)
        .filter(|end| scope.count > 0 && *end <= before.doc.page_count())
        .ok_or("Invalid facing batch range")?;
    if before.uid != after.uid || before.doc.spreads.len() != after.doc.spreads.len() || before.doc.page_count() != after.doc.page_count() {
        return Err("Facing batch cannot replace or rearrange document pages".into());
    }
    // Bound tree depth before any recursive equality/bounds operation on a document node.
    validate_item_trees(before)?;
    validate_item_trees(after)?;
    let mut absolute = 0usize;
    for (old, new) in before.doc.spreads.iter().zip(&after.doc.spreads) {
        if old.id != new.id || old.pages != new.pages || old.parent != new.parent || old.allow_shuffle != new.allow_shuffle {
            return Err("Facing batch changed page geometry, order or spread metadata".into());
        }
        let old_ids: std::collections::BTreeSet<_> = old.items.iter().map(|item| item.id).collect();
        let preserved: Vec<_> = new.items.iter().filter(|item| old_ids.contains(&item.id)).collect();
        if old.items.iter().ne(preserved) {
            return Err("Facing batch changed or rearranged existing native artwork".into());
        }
        for item in &old.items {
            let bounds = item.visible_bounds();
            let assigned = old.page_at_x(item.bounds().center().x).ok_or("Existing artwork has no page")?;
            let assigned = absolute.checked_add(assigned).ok_or("Facing artwork page overflow")?;
            if (start..end).contains(&assigned)
                || old.pages.iter().enumerate().any(|(offset, page)| {
                    absolute.checked_add(offset).is_some_and(|index| (start..end).contains(&index)) && overlaps(bounds, page.bounds())
                })
            {
                return Err("Facing batch target pages must be blank before production".into());
            }
        }
        for item in new.items.iter().filter(|item| !old_ids.contains(&item.id)) {
            let bounds = item.visible_bounds();
            if ![bounds.x0, bounds.y0, bounds.x1, bounds.y1].iter().all(|number| number.is_finite()) {
                return Err("Facing batch produced non-finite native geometry".into());
            }
            let assigned = new.page_at_x(item.bounds().center().x).ok_or("New artwork has no page")?;
            let assigned = absolute.checked_add(assigned).ok_or("Facing artwork page overflow")?;
            if !(start..end).contains(&assigned)
                || new.pages.iter().enumerate().any(|(offset, page)| {
                    absolute.checked_add(offset).is_none_or(|index| !(start..end).contains(&index)) && overlaps(bounds, page.bounds())
                })
            {
                return Err("Facing batch placed native artwork on an unapproved absolute page".into());
            }
        }
        absolute = absolute.checked_add(old.pages.len()).ok_or("Facing page count overflow")?;
    }
    for (id, story) in &before.doc.stories {
        if after.doc.stories.get(id) != Some(story) {
            return Err("Facing batch changed an existing story".into());
        }
    }
    for (id, asset) in &before.doc.assets {
        if after.doc.assets.get(id) != Some(asset) {
            return Err("Facing batch changed an existing asset".into());
        }
    }
    let mut normalized = after.doc.as_ref().clone();
    normalized.spreads = before.doc.spreads.clone();
    normalized.stories = before.doc.stories.clone();
    normalized.assets = before.doc.assets.clone();
    normalized.next_id = before.doc.next_id;
    normalized.modified = before.doc.modified;
    if &normalized != before.doc.as_ref() {
        return Err("Facing batch changed global styles, swatches, layers or document settings".into());
    }
    Ok(())
}

fn validate_item_trees(state: &DocState) -> Result<(), String> {
    let mut visited = 0usize;
    let mut identifiers = std::collections::BTreeSet::new();
    for spread in state.doc.spreads.iter().chain(&state.doc.parents) {
        if spread.items.len() > 100_000 {
            return Err("Facing spread exceeds its native item count bound".into());
        }
        let mut pending: Vec<_> = spread.items.iter().map(|item| (item.as_ref(), 0u32)).collect();
        while let Some((item, depth)) = pending.pop() {
            visited = visited.checked_add(1).ok_or("Facing artwork count overflow")?;
            if visited > 100_000 || depth > 64 || !identifiers.insert(item.id) {
                return Err("Facing artwork exceeds its tree bound or repeats an item ID".into());
            }
            let children = item.children();
            if pending.len().checked_add(children.len()).is_none_or(|count| count > 100_000) {
                return Err("Facing artwork pending tree exceeds its bound".into());
            }
            for child in children {
                pending.push((child.as_ref(), depth.saturating_add(1)));
            }
        }
    }
    Ok(())
}

fn overlaps(a: designcraft_geom::Rect, b: designcraft_geom::Rect) -> bool {
    a.x0 < b.x1 && a.x1 > b.x0 && a.y0 < b.y1 && a.y1 > b.y0
}

/// Content-language metadata takes priority over the language used to write the brief.
/// Mixed English/Arabic content follows the Arabic book default; each text story retains its
/// own direction. Explicit binding metadata always wins over a script-based fallback.
pub(crate) fn auto_direction(brief: &str) -> &'static str {
    let mut content = None;
    for line in brief.lines() {
        let lower = line.to_lowercase();
        if lower.contains("binding") || lower.contains("اتجاه التجليد") || lower.contains("اتجاه الكتاب") {
            if lower.contains("righttoleft") || lower.contains("right-to-left") || lower.contains("rtl") || lower.contains("من اليمين") {
                return "rightToLeft";
            }
            if lower.contains("lefttoright") || lower.contains("left-to-right") || lower.contains("ltr") || lower.contains("من اليسار") {
                return "leftToRight";
            }
        }
        if lower.contains("content copy language")
            || lower.contains("content language")
            || lower.contains("لغة المحتوى")
            || lower.contains("لغة الكتاب")
        {
            let value = lower.split_once(':').map_or(lower.as_str(), |(_, value)| value);
            content = Some(if mentions_arabic(value) { "rightToLeft" } else { "leftToRight" });
        }
    }
    content.unwrap_or_else(|| {
        let lower = brief.to_lowercase();
        if ["باللغة الإنجليزية", "باللغة الانجليزية", "باللغة الإنكليزية", "باللغة الانكليزية", "english-only", "english only"]
            .iter()
            .any(|phrase| lower.contains(*phrase))
        {
            "leftToRight"
        } else if brief.chars().any(arabic_letter) {
            "rightToLeft"
        } else {
            "leftToRight"
        }
    })
}

fn mentions_arabic(text: &str) -> bool {
    if text.contains("arabic") || text.contains("العربي") || text.contains("بالعربي") || text.contains("عربية") {
        return true;
    }
    if ["english", "إنجليزي", "انجليزي", "إنكليزي", "انكليزي"].iter().any(|word| text.contains(*word)) {
        return false;
    }
    text.chars().any(arabic_letter)
}

fn arabic_letter(character: char) -> bool {
    character.is_alphabetic()
        && matches!(u32::from(character), 0x0600..=0x06ff | 0x0750..=0x077f | 0x08a0..=0x08ff | 0xfb50..=0xfdff | 0xfe70..=0xfeff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use designcraft_ai_types::EngineAction;

    fn book(pages: usize, rtl: bool) -> Session {
        let mut session = Session::new();
        session
            .execute(
                "file.new",
                &json!({"width":400,"height":600,"pages":pages,"facingPages":true,"margins":{"top":15,"bottom":15,"inside":18,"outside":15}}),
            )
            .unwrap();
        session.execute("layout.documentSetup", &json!({"binding":if rtl {"rightToLeft"} else {"leftToRight"}})).unwrap();
        session
    }

    fn action(command: &str, params: Value) -> EngineAction {
        EngineAction { command: command.into(), params }
    }

    #[test]
    fn actual_rtl_book_has_pairs_and_canonical_pages_keep_ids_and_artwork() {
        let mut session = book(48, true);
        assert_eq!(session.doc().unwrap().doc.spreads.len(), 25);
        let actual_page = session.doc().unwrap().doc.page(1).unwrap().clone();
        let spread = session.doc().unwrap().doc.page_loc(1).unwrap().0;
        assert_eq!(actual_page.x, 400.0);
        let made = session.execute("frame.create", &json!({"spread":spread,"rect":[actual_page.x+20.0,30,actual_page.x+120.0,130]})).unwrap();
        let before = session.doc().unwrap().clone();
        let canonical = canonicalize(&before, &session.prefs).unwrap();
        assert_eq!((canonical.uid, canonical.revision), (before.uid, before.revision));
        assert_eq!(canonical.doc.spreads.len(), 48);
        assert!(!canonical.doc.settings.facing_pages);
        assert_eq!(canonical.doc.page(1).unwrap().id, actual_page.id);
        for absolute in 0..48 {
            let actual = before.doc.page(absolute).unwrap();
            let local = canonical.doc.page(absolute).unwrap();
            assert_eq!(local.margin_rect().x0, actual.margin_rect().x0 - actual.x);
            assert_eq!(local.margin_rect().x1, actual.margin_rect().x1 - actual.x);
        }
        let item = canonical.doc.item(doc::ItemId(made["id"].as_u64().unwrap())).unwrap();
        assert_eq!(item.bounds().x0, 20.0);
        assert_eq!(before.doc.page(1).unwrap().x, 400.0);
        assert!(before.doc.settings.facing_pages);
        let proof = formatted_proof_state(&canonical, &session.prefs, true).unwrap();
        assert_eq!(proof.doc.spreads.len(), 25);
        assert!(proof.doc.settings.right_to_left_binding);
        assert_eq!(proof.doc.page(1).unwrap().id, actual_page.id);
        assert_eq!(proof.doc.item(doc::ItemId(made["id"].as_u64().unwrap())).unwrap().bounds().x0, 420.0);
    }

    #[test]
    fn maps_native_geometry_references_and_relative_values_without_extra_actions() {
        let session = book(4, true);
        let plan = ProductionPlan {
            title: "Facing native proof".into(),
            images: vec![],
            commands: vec![
                action("frame.create", json!({"spread":1,"rect":[20,30,120,130]})),
                action("transform.move", json!({"ids":["$0.id"],"dx":5,"dy":7})),
                action("transform.set", json!({"ids":["${0.id}"],"x":25,"y":37})),
                action("line.create", json!({"spread":1,"a":[20,150],"b":[120,150]})),
                action("path.create", json!({"spread":2,"anchors":[{"p":[20,200],"in":[10,200],"out":[30,200]},{"p":[80,240]}]})),
                action("object.fittingOptions", json!({"ids":["$0.id"],"fitting":"fitProportionally","crop":[1,2,3,4]})),
            ],
        };
        let mapped = map_plan(&plan, session.doc().unwrap()).unwrap();
        assert_eq!(mapped.commands.len(), plan.commands.len());
        assert_eq!(mapped.commands[0].params["spread"], 1);
        assert_eq!(mapped.commands[0].params["rect"], json!([420.0, 30, 520.0, 130]));
        assert_eq!(mapped.commands[1], plan.commands[1]);
        assert_eq!(mapped.commands[2].params["x"], 425.0);
        assert_eq!(mapped.commands[2].params["ids"], json!(["${0.id}"]));
        assert_eq!(mapped.commands[3].params["a"], json!([420.0, 150]));
        assert_eq!(mapped.commands[4].params["spread"], 1);
        assert_eq!(mapped.commands[4].params["anchors"][0]["in"], json!([10.0, 200]));
        assert_eq!(mapped.commands[5], plan.commands[5]);
    }

    #[test]
    fn page_ruler_absolute_transform_is_already_page_local() {
        let mut session = book(4, true);
        Arc::make_mut(&mut session.doc_mut().unwrap().doc).settings.ruler_origin = doc::RulerOrigin::Page;
        let plan = ProductionPlan {
            title: "Local ruler".into(),
            images: vec![],
            commands: vec![
                action("frame.create", json!({"spread":1,"rect":[20,30,120,130]})),
                action("transform.set", json!({"ids":["$0.id"],"x":25})),
            ],
        };
        let mapped = map_plan(&plan, session.doc().unwrap()).unwrap();
        assert_eq!(mapped.commands[1].params["x"], 25.0);
    }

    #[test]
    fn mapped_plan_executes_in_real_facing_document_at_the_same_local_position() {
        for rtl in [false, true] {
            let mut session = book(4, rtl);
            let plan = ProductionPlan {
                title: "Native mapping".into(),
                images: vec![],
                commands: vec![
                    action("frame.create", json!({"spread":1,"rect":[20,30,120,130]})),
                    action("transform.set", json!({"ids":["$0.id"],"x":35,"y":45})),
                ],
            };
            let mapped = map_plan(&plan, session.doc().unwrap()).unwrap();
            session.execute("production.apply", &json!({"commands":mapped.commands,"outputMode":"native_only","requirePreflight":false})).unwrap();
            let canonical = canonicalize(session.doc().unwrap(), &session.prefs).unwrap();
            assert_eq!(canonical.doc.spreads[1].items.len(), 1);
            assert_eq!(canonical.doc.spreads[1].items[0].bounds().x0, 35.0);
            assert_eq!(canonical.doc.spreads[1].items[0].bounds().y0, 45.0);
        }
    }

    #[test]
    fn metadata_uses_content_language_and_explicit_binding_before_brief_script() {
        assert_eq!(auto_direction("Script Language: English\nContent Copy Language: English LTR + Vocalized Arabic RTL"), "rightToLeft");
        assert_eq!(auto_direction("صمّم هذا الكتاب\nContent Copy Language: English"), "leftToRight");
        assert_eq!(auto_direction("لغة المحتوى: الإنجليزية"), "leftToRight");
        assert_eq!(auto_direction("صمّم الكتاب باللغة الإنجليزية"), "leftToRight");
        assert_eq!(auto_direction("لغة المحتوى: العربية\nBinding: leftToRight"), "leftToRight");
        assert_eq!(auto_direction("كتاب عربي واضح"), "rightToLeft");
        assert_eq!(auto_direction("An English workbook"), "leftToRight");
    }

    #[test]
    fn invalid_page_or_reference_fails_without_changing_the_live_document() {
        let session = book(4, true);
        for commands in [
            vec![action("frame.create", json!({"spread":4,"rect":[0,0,20,20]}))],
            vec![action("transform.set", json!({"ids":["$9.id"],"x":10}))],
            vec![action("frame.create", json!({"spread":1,"rect":[15500,0,15552,20]}))],
        ] {
            let plan = ProductionPlan { title: "Invalid".into(), images: vec![], commands };
            assert!(map_plan(&plan, session.doc().unwrap()).is_err());
        }
        assert_eq!(session.doc().unwrap().doc.spreads.len(), 3);
        assert!(session.doc().unwrap().doc.spreads.iter().all(|spread| spread.items.is_empty()));
    }

    #[test]
    fn facing_six_page_batch_preserves_cover_and_neighbour_and_rejects_escape() {
        let mut session = book(6, true);
        session.execute("frame.create", &json!({"spread":0,"rect":[20,20,80,80]})).unwrap();
        let before = session.doc().unwrap().clone();
        let scope = crate::worker::BatchScope { start: 1, count: 2 };
        let plan = ProductionPlan {
            title: "Opposite pages two and three".into(),
            images: vec![],
            commands: vec![
                action("frame.create", json!({"spread":1,"rect":[20,30,120,130]})),
                action("frame.create", json!({"spread":2,"rect":[20,30,120,130]})),
            ],
        };
        let mapped = map_plan(&plan, &before).unwrap();
        session.execute("production.apply", &json!({"commands":mapped.commands,"outputMode":"native_only","requirePreflight":false})).unwrap();
        let after = session.doc().unwrap().clone();
        assert!(validate_mapped_result(&before, &after, scope).is_ok());
        assert_eq!(after.doc.spreads.len(), 4);
        assert_eq!(before.doc.spreads[0], after.doc.spreads[0]);
        assert_eq!(before.doc.spreads[2], after.doc.spreads[2]);
        let mut escaped = after.clone();
        Arc::make_mut(&mut Arc::make_mut(&mut escaped.doc).spreads[1]).pages[0].margins.inside += 1.0;
        assert!(validate_mapped_result(&before, &escaped, scope).is_err());
        let mut extra = Session::new();
        extra.add_document(after.clone());
        extra.execute("frame.create", &json!({"spread":2,"rect":[420,30,520,130]})).unwrap();
        assert!(validate_mapped_result(&before, extra.doc().unwrap(), scope).is_err());
    }
}
