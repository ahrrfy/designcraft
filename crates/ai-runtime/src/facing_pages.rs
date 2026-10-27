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

pub(crate) fn single_page_proof_state(source: &DocState, prefs: &Prefs, right_to_left: bool) -> Result<DocState, String> {
    validate_pages(source)?;
    validate_item_trees(source)?;
    if !source.doc.settings.facing_pages && source.doc.settings.right_to_left_binding == right_to_left {
        return Ok(source.clone());
    }
    setup(source, prefs, false, right_to_left)
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
    if actual.doc.settings.facing_pages {
        mirror_facing_footer_outer(&mut mapped.commands, actual)?;
    }
    let mut result_pages: Vec<StepPages> = Vec::with_capacity(plan.commands.len());
    let mut selected: Vec<usize> = Vec::new();
    for (index, action) in mapped.commands.iter_mut().enumerate() {
        let params = action.params.as_object_mut().ok_or("Facing-page command parameters must be an object")?;
        let qr_creation = action.command == "object.qrCode" && params.contains_key("rect");
        let thread_creation = action.command == "text.thread" && params.contains_key("rect");
        let creation = matches!(action.command.as_str(), "frame.create" | "line.create" | "path.create") || qr_creation || thread_creation;
        let standalone_image = action.command == "file.place" && !params.contains_key("frame");
        let mut step_pages = StepPages::default();
        if creation || standalone_image {
            if thread_creation {
                match (params.get("from"), params.get("to")) {
                    (Some(from_val), None) => {
                        step_pages.from = Some(single_target_page(from_val, &result_pages, index, actual)?);
                    }
                    (None, Some(to_val)) => {
                        step_pages.to = Some(single_target_page(to_val, &result_pages, index, actual)?);
                    }
                    _ => return Err("Projected text.thread with rect requires exactly one of from or to".into()),
                }
            }
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
            if thread_creation {
                if step_pages.from.is_some() {
                    step_pages.to = Some(absolute);
                } else {
                    step_pages.from = Some(absolute);
                }
            } else {
                step_pages.id = Some(absolute);
            }
        } else if action.command == "file.place" {
            let frame_val = params.get("frame").ok_or("Projected file.place requires frame")?;
            let page = single_target_page(frame_val, &result_pages, index, actual)?;
            selected = vec![page];
            step_pages.id = Some(page);
        } else if action.command == "text.thread" {
            let from_val = params.get("from").ok_or("Projected text.thread requires from")?;
            let to_val = params.get("to").ok_or("Projected text.thread requires to")?;
            let from_page = single_target_page(from_val, &result_pages, index, actual)?;
            let to_page = single_target_page(to_val, &result_pages, index, actual)?;
            selected = vec![to_page];
            step_pages.from = Some(from_page);
            step_pages.to = Some(to_page);
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
        } else if matches!(action.command.as_str(), "object.group" | "object.pathfinder" | "object.makeCompoundPath") {
            let pages = match params.get("ids") {
                Some(ids) => target_pages(Some(ids), &result_pages, index, actual)?,
                None => selected.clone(),
            };
            if pages.len() != 1 {
                return Err("Projected groups must remain on one absolute page".into());
            }
            selected = pages.clone();
            step_pages.id = pages.first().copied();
        } else if action.command == "object.ungroup" {
            let pages = match params.get("ids") {
                Some(ids) => target_pages(Some(ids), &result_pages, index, actual)?,
                None => selected.clone(),
            };
            if pages.len() != 1 {
                return Err("Projected ungrouped objects must remain on one absolute page".into());
            }
            selected = pages;
        } else if matches!(action.command.as_str(), "object.releaseCompoundPath" | "edit.stepAndRepeat") {
            let pages = match params.get("ids") {
                Some(ids) => target_pages(Some(ids), &result_pages, index, actual)?,
                None => selected.clone(),
            };
            if pages.len() != 1 {
                return Err("Projected compound or repeated objects must remain on one absolute page".into());
            }
            selected = pages.clone();
            step_pages.ids = pages.first().copied();
        } else if action.command == "object.altText" {
            let pages = match params.get("ids") {
                Some(ids) => target_pages(Some(ids), &result_pages, index, actual)?,
                None => selected.clone(),
            };
            if pages.is_empty() {
                return Err("Projected object.altText requires a target object".into());
            }
        } else if action.command == "object.caption" {
            let mut pages = match params.get("ids") {
                Some(ids) => target_pages(Some(ids), &result_pages, index, actual)?,
                None => selected.clone(),
            };
            if let Some(cf) = params.get("captionFrame").filter(|v| !v.is_null()) {
                let cf_page = single_target_page(cf, &result_pages, index, actual)?;
                if !pages.contains(&cf_page) {
                    pages.push(cf_page);
                }
            }
            if pages.len() != 1 {
                return Err("Projected captions must remain on one absolute page".into());
            }
            selected = pages.clone();
            step_pages.ids = pages.first().copied();
            if params.get("group").and_then(Value::as_bool) == Some(true) {
                step_pages.groups = pages.first().copied();
            }
        } else if action.command == "layout.section" {
            let page_1 = params.get("page").and_then(Value::as_u64).ok_or("Projected layout.section requires a 1-based page")?;
            let abs = usize::try_from(page_1).ok().and_then(|n| n.checked_sub(1)).ok_or("Projected layout.section page is out of range")?;
            page_location(actual, abs)?;
        } else if action.command == "layout.pages.applyParent" {
            let arr =
                params.get("pages").and_then(Value::as_array).filter(|a| !a.is_empty()).ok_or("Projected layout.pages.applyParent requires pages")?;
            for v in arr {
                let page_0 = v.as_u64().ok_or("Projected layout.pages.applyParent page must be a 0-based integer")?;
                let abs = usize::try_from(page_0).map_err(|_| "Projected layout.pages.applyParent page is out of range")?;
                page_location(actual, abs)?;
            }
        } else if action.command == "object.qrCode" {
            params.remove("spread");
            let page = *selected.first().ok_or("Projected object.qrCode without rect requires a selected target")?;
            selected = vec![page];
            step_pages.id = Some(page);
        } else if matches!(action.command.as_str(), "object.align" | "object.distribute") {
            let pages = match params.get("ids") {
                Some(ids) => target_pages(Some(ids), &result_pages, index, actual)?,
                None => selected.clone(),
            };
            if pages.len() != 1 {
                return Err("Projected alignment or distribution cannot combine different pages".into());
            }
            if action.command == "object.align" && params.get("to").and_then(Value::as_str) == Some("spread") {
                params.insert("to".into(), json!("page"));
            }
        } else if action.command == "transform.move" {
            if params.contains_key("toSpread") {
                return Err("Projected transform.move cannot move objects across spreads".into());
            }
            let pages = match params.get("ids") {
                Some(ids) => target_pages(Some(ids), &result_pages, index, actual)?,
                None => selected.clone(),
            };
            if pages.len() == 1 {
                step_pages.ids = pages.first().copied();
            }
            if params.get("copy").and_then(Value::as_bool) == Some(true) {
                selected = pages;
            }
        } else if action.command == "transform.set" {
            let pages = match params.get("ids") {
                Some(ids) => target_pages(Some(ids), &result_pages, index, actual)?,
                None => selected.clone(),
            };
            if params.contains_key("x") || params.contains_key("bounds") {
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
                if let Some(bounds) = params.get_mut("bounds") {
                    translate_array(bounds, 4, &[0, 2], page.x)?;
                }
            }
        } else if matches!(action.command.as_str(), "object.gradient" | "object.gradientFeather")
            && (params.contains_key("from") || params.contains_key("to"))
        {
            let pages = match params.get("ids") {
                Some(ids) => target_pages(Some(ids), &result_pages, index, actual)?,
                None => selected.clone(),
            };
            let absolute = *pages.first().ok_or("Projected gradient vector has no target page")?;
            if pages.iter().any(|page| *page != absolute) {
                return Err("Projected gradient vectors cannot combine different projected pages".into());
            }
            let (_, page) = page_location(actual, absolute)?;
            for key in ["from", "to"] {
                if let Some(point) = params.get_mut(key) {
                    translate_array(point, 2, &[0], page.x)?;
                }
            }
        } else if action.command == "object.effects"
            && params
                .get("effects")
                .and_then(|e| e.get("gradientFeather"))
                .and_then(Value::as_object)
                .is_some_and(|gf| gf.contains_key("from") || gf.contains_key("to") || gf.get("vector").is_some_and(|v| !v.is_null()))
        {
            let pages = match params.get("ids") {
                Some(ids) => target_pages(Some(ids), &result_pages, index, actual)?,
                None => selected.clone(),
            };
            let absolute = *pages.first().ok_or("Projected effects gradientFeather vector has no target page")?;
            if pages.iter().any(|page| *page != absolute) {
                return Err("Projected effects gradientFeather vectors cannot combine different projected pages".into());
            }
            let (_, page) = page_location(actual, absolute)?;
            if let Some(gf) =
                params.get_mut("effects").and_then(Value::as_object_mut).and_then(|e| e.get_mut("gradientFeather")).and_then(Value::as_object_mut)
            {
                if let Some(mut vec_val) = gf.remove("vector")
                    && !vec_val.is_null()
                {
                    translate_array(&mut vec_val, 4, &[], 0.0)?;
                    if let Some([x0, y0, x1, y1]) = vec_val.as_array().map(Vec::as_slice) {
                        gf.entry("from").or_insert_with(|| json!([x0.clone(), y0.clone()]));
                        gf.entry("to").or_insert_with(|| json!([x1.clone(), y1.clone()]));
                    }
                }
                if gf.contains_key("from") != gf.contains_key("to") {
                    return Err("Projected effects gradientFeather requires both from and to endpoints".into());
                }
                for key in ["from", "to"] {
                    if let Some(point) = gf.get_mut(key) {
                        translate_array(point, 2, &[0], page.x)?;
                    }
                }
            }
        } else if matches!(action.command.as_str(), "style.object.create" | "style.object.edit")
            && [params.get("effects").and_then(|e| e.get("gradientFeather")), params.get("gradientFeather")].into_iter().flatten().any(|gf| {
                gf.as_object().is_some_and(|o| o.contains_key("from") || o.contains_key("to") || o.get("vector").is_some_and(|v| !v.is_null()))
            })
        {
            return Err(
                "Projected object styles cannot bind page-local gradientFeather endpoints across facing pages; use angle or object.gradientFeather"
                    .into(),
            );
        }
        result_pages.push(step_pages);
    }
    Ok(mapped)
}

fn mirror_facing_footer_outer(commands: &mut [designcraft_ai_types::EngineAction], actual: &DocState) -> Result<(), String> {
    let mut spine_footer_pages = std::collections::BTreeSet::new();
    let len = commands.len();
    for j in 0..len {
        let j2 = j.saturating_add(2);
        let j3 = j.saturating_add(3);
        let is_footer_page = commands.get(j).is_some_and(|a| a.command == "frame.create")
            && commands.get(j2).is_some_and(|a| a.command == "type.para")
            && commands
                .get(j3)
                .is_some_and(|a| a.command == "object.label" && a.params.get("label").and_then(Value::as_str) == Some("part.ui.footer.page"));
        if !is_footer_page {
            continue;
        }
        let Some(frame_cmd) = commands.get(j) else {
            continue;
        };
        let Some(absolute_u64) = frame_cmd.params.get("spread").and_then(Value::as_u64) else {
            continue;
        };
        let Ok(absolute) = usize::try_from(absolute_u64) else {
            continue;
        };
        let (_, page) = page_location(actual, absolute)?;
        let Some((x0, x1)) = frame_cmd
            .params
            .get("rect")
            .and_then(Value::as_array)
            .filter(|r| r.len() == 4)
            .and_then(|r| Some((r.first()?.as_f64()?, r.get(2)?.as_f64()?)))
        else {
            continue;
        };
        let cx = (x0 + x1) * 0.5;
        let on_spine = match page.side {
            doc::PageSide::Left => cx > page.width * 0.5,
            doc::PageSide::Right => cx < page.width * 0.5,
            doc::PageSide::Single => false,
        };
        if !on_spine {
            continue;
        }
        spine_footer_pages.insert(absolute_u64);
        if let Some(rect) = commands.get_mut(j).and_then(|cmd| cmd.params.get_mut("rect")).and_then(Value::as_array_mut) {
            if let Some(s0) = rect.get_mut(0) {
                *s0 = json!(page.width - x1);
            }
            if let Some(s2) = rect.get_mut(2) {
                *s2 = json!(page.width - x0);
            }
        }
        if let Some(attrs) = commands.get_mut(j2).and_then(|cmd| cmd.params.get_mut("attrs")).and_then(Value::as_object_mut) {
            let align = if page.side == doc::PageSide::Left { "left" } else { "right" };
            attrs.insert("align".into(), json!(align));
        }
    }
    if spine_footer_pages.is_empty() {
        return Ok(());
    }
    for i in 0..len {
        let i2 = i.saturating_add(2);
        let is_footer_bar = commands.get(i).is_some_and(|a| a.command == "path.create")
            && commands
                .get(i2)
                .is_some_and(|a| a.command == "object.label" && a.params.get("label").and_then(Value::as_str) == Some("part.ui.footer.bar"));
        if !is_footer_bar {
            continue;
        }
        let Some(absolute_u64) = commands.get(i).and_then(|cmd| cmd.params.get("spread")).and_then(Value::as_u64) else {
            continue;
        };
        if !spine_footer_pages.contains(&absolute_u64) {
            continue;
        }
        let Ok(absolute) = usize::try_from(absolute_u64) else {
            continue;
        };
        let (_, page) = page_location(actual, absolute)?;
        if let Some(anchors) = commands.get_mut(i).and_then(|cmd| cmd.params.get_mut("anchors")).and_then(Value::as_array_mut) {
            for anchor in anchors {
                if let Some(obj) = anchor.as_object_mut() {
                    for key in ["p", "in", "out"] {
                        if let Some(pt) = obj.get_mut(key).and_then(Value::as_array_mut)
                            && let Some(x) = pt.first().and_then(Value::as_f64)
                            && let Some(slot0) = pt.get_mut(0)
                        {
                            *slot0 = json!(page.width - x);
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct StepPages {
    id: Option<usize>,
    from: Option<usize>,
    to: Option<usize>,
    ids: Option<usize>,
    groups: Option<usize>,
}

fn page_location(actual: &DocState, absolute: usize) -> Result<(usize, &doc::Page), String> {
    let (spread, page) = actual.doc.page_loc(absolute).ok_or("Projected object targets a missing absolute page")?;
    let page = actual.doc.spreads.get(spread).and_then(|spread| spread.pages.get(page)).ok_or("Projected page disappeared")?;
    Ok((spread, page))
}

fn single_target_page(id: &Value, results: &[StepPages], current: usize, actual: &DocState) -> Result<usize, String> {
    if let Some(reference) = id.as_str() {
        let reference = reference.strip_prefix("${").and_then(|text| text.strip_suffix('}')).or_else(|| reference.strip_prefix('$'));
        let (step, field) = reference.and_then(|text| text.split_once('.')).ok_or("Invalid projected result reference")?;
        let step = if step == "last" {
            current.checked_sub(1).ok_or("Projected object targets must reference an earlier object ID")?
        } else {
            step.parse::<usize>().map_err(|_| "Invalid projected result index")?
        };
        if step >= current {
            return Err("Projected object targets must reference an earlier object ID".into());
        }
        let step_pages = results.get(step).ok_or("Projected reference has no target page")?;
        let root_field = field.split('.').next().unwrap_or(field);
        let resolved = match root_field {
            "id" => step_pages.id,
            "ids" => step_pages.ids,
            "groups" => step_pages.groups,
            "from" => step_pages.from,
            "to" => step_pages.to,
            _ => return Err("Projected object targets must reference an earlier object ID".into()),
        };
        resolved.ok_or_else(|| "Projected reference has no target page".into())
    } else if let Some(id) = id.as_u64() {
        let location = actual.doc.find(doc::ItemId(id)).ok_or("Projected literal target does not exist")?;
        let doc::SpreadRef::Doc(spread) = location.spread else {
            return Err("Projected transforms cannot target parent items".into());
        };
        let spread_ref = actual.doc.spreads.get(spread).ok_or("Projected target spread disappeared")?;
        let item = actual.doc.item(doc::ItemId(id)).ok_or("Projected target disappeared")?;
        let page = spread_ref.page_at_x(item.bounds().center().x).ok_or("Projected target page disappeared")?;
        actual.doc.first_page_of_spread(spread).checked_add(page).ok_or_else(|| "Projected target page overflow".into())
    } else {
        Err("Projected targets require object IDs or result references".into())
    }
}

fn target_pages(ids: Option<&Value>, results: &[StepPages], current: usize, actual: &DocState) -> Result<Vec<usize>, String> {
    match ids {
        Some(Value::Array(items)) => {
            if items.len() > 1024 {
                return Err("Projected target count exceeds its limit".into());
            }
            let mut pages = Vec::new();
            for id in items {
                let page = single_target_page(id, results, current, actual)?;
                if !pages.contains(&page) {
                    pages.push(page);
                }
            }
            Ok(pages)
        }
        Some(single @ (Value::String(_) | Value::Number(_))) => Ok(vec![single_target_page(single, results, current, actual)?]),
        _ => Err("Projected targets must be an ID array".into()),
    }
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
        if old.id != new.id
            || !pages_equal_allowing_scoped_parent(&old.pages, &new.pages, absolute, start, end)
            || old.parent != new.parent
            || old.allow_shuffle != new.allow_shuffle
        {
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
    if crate::worker::marker_only_sections_within_scope(&before.doc.sections, &normalized.sections, start, end) {
        normalized.sections = before.doc.sections.clone();
    }
    if normalized.swatches.len() >= before.doc.swatches.len()
        && normalized.swatches.get(..before.doc.swatches.len()) == Some(before.doc.swatches.as_slice())
        && normalized.swatches.get(before.doc.swatches.len()..).is_some_and(|extra| {
            extra.iter().all(|s| !s.named && s.hidden && matches!(s.value, designcraft_engine::doc::color::SwatchValue::Gradient { .. }))
        })
    {
        normalized.swatches.truncate(before.doc.swatches.len());
    }
    if &normalized != before.doc.as_ref() {
        return Err("Facing batch changed global styles, swatches, layers or document settings".into());
    }
    Ok(())
}

pub(crate) fn pages_equal_allowing_scoped_parent(old: &[doc::Page], new: &[doc::Page], spread_abs: usize, start: usize, end: usize) -> bool {
    if old.len() != new.len() {
        return false;
    }
    old.iter().zip(new).enumerate().all(|(offset, (a, b))| {
        if spread_abs.checked_add(offset).is_some_and(|abs| (start..end).contains(&abs)) {
            let mut normalized = b.clone();
            normalized.parent = a.parent;
            &normalized == a
        } else {
            a == b
        }
    })
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

    #[test]
    fn maps_qr_threaded_gradient_and_repeated_copy_ids_on_facing_spreads() {
        let mut session = book(6, true);
        let before = session.doc().unwrap().clone();
        let scope = crate::worker::BatchScope { start: 1, count: 2 };
        let tiny_png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let plan = ProductionPlan {
            title: "Rich facing batch".into(),
            images: vec![],
            commands: vec![
                action("frame.create", json!({"spread":1,"content":"text","rect":[20,30,180,90],"text":"Threaded start"})),
                action("text.thread", json!({"from":"$0.id","spread":2,"rect":[20,30,180,90]})),
                action("transform.set", json!({"ids":["$1.to"],"x":24})),
                action("object.qrCode", json!({"spread":1,"rect":[200,30,270,100],"content":"https://example.org"})),
                action("transform.set", json!({"ids":["$3.id"],"x":205})),
                action("frame.create", json!({"spread":1,"content":"unassigned","rect":[20,120,180,200]})),
                action(
                    "object.gradient",
                    json!({
                        "ids":["$5.id"],
                        "kind":"linear",
                        "from":[20,120],
                        "to":[180,120],
                        "stops":[{"color":"#1E3A8A","location":0},{"color":"#E0F2FE","location":100}]
                    }),
                ),
                action("frame.create", json!({"spread":2,"content":"graphic","rect":[20,120,80,180]})),
                action("file.place", json!({"frame":"$7.id","base64":tiny_png,"name":"dot.png"})),
                action("transform.move", json!({"ids":["$7.id"],"dx":80,"dy":0,"copy":true})),
                action("transform.set", json!({"ids":"$9.ids","width":55,"height":55})),
            ],
        };
        let mapped = map_plan(&plan, &before).unwrap();
        assert_eq!(mapped.commands[1].params["spread"], 1);
        assert_eq!(mapped.commands[1].params["rect"], json!([20.0, 30, 180.0, 90]));
        assert_eq!(mapped.commands[2].params["x"], 24.0);
        assert_eq!(mapped.commands[3].params["spread"], 1);
        assert_eq!(mapped.commands[3].params["rect"], json!([600.0, 30, 670.0, 100]));
        assert_eq!(mapped.commands[4].params["x"], 605.0);
        assert_eq!(mapped.commands[6].params["from"], json!([420.0, 120]));
        assert_eq!(mapped.commands[6].params["to"], json!([580.0, 120]));
        session.execute("production.apply", &json!({"commands":mapped.commands,"outputMode":"native_with_images","requirePreflight":false})).unwrap();
        let after = session.doc().unwrap().clone();
        validate_mapped_result(&before, &after, scope).unwrap();
    }

    #[test]
    fn maps_dotted_ids_last_refs_selected_qr_and_page_local_spread_alignment() {
        let mut session = book(6, true);
        let before = session.doc().unwrap().clone();
        let scope = crate::worker::BatchScope { start: 1, count: 2 };
        let plan = ProductionPlan {
            title: "Dotted, last, rect-less QR, and spread align".into(),
            images: vec![],
            commands: vec![
                action("frame.create", json!({"spread":1,"content":"unassigned","rect":[30,40,90,100]})),
                action("edit.stepAndRepeat", json!({"ids":["$0.id"],"count":2,"dx":70,"dy":0})),
                action("transform.set", json!({"ids":["$1.ids.0"],"x":105})),
                action("frame.create", json!({"spread":2,"content":"unassigned","rect":[40,140,120,220]})),
                action("object.qrCode", json!({"type":"email","to":"teacher@example.org","subject":"Quiz","body":"Page 3"})),
                action("transform.set", json!({"ids":["$last.id"],"x":45})),
                action("object.align", json!({"ids":["$3.id"],"edge":"right","to":"spread"})),
            ],
        };
        let mapped = map_plan(&plan, &before).unwrap();
        assert_eq!(mapped.commands[2].params["x"], 505.0, "$1.ids.0 on RTL page 1 (x=400) translates by +400");
        assert_eq!(mapped.commands[5].params["x"], 45.0, "$last.id on RTL page 2 (x=0) stays at 45");
        assert_eq!(mapped.commands[6].params["to"], "page", "draft spread alignment must stay on its projected page");
        session.execute("production.apply", &json!({"commands":mapped.commands,"outputMode":"native_only","requirePreflight":false})).unwrap();
        let after = session.doc().unwrap().clone();
        validate_mapped_result(&before, &after, scope).unwrap();
        let aligned_qr_frame = after.doc.spreads[1].items.iter().find(|it| it.bounds().y0 == 140.0).unwrap();
        assert!((aligned_qr_frame.bounds().x1 - 400.0).abs() < 0.01, "aligned to right edge of page 2 (x=0..400), not full spread (0..800)");

        // Cross-page alignment and $ungroup.ids must fail cleanly before touching the live document.
        let cross_align = ProductionPlan {
            title: "Cross-page align".into(),
            images: vec![],
            commands: vec![
                action("frame.create", json!({"spread":1,"rect":[20,30,80,90]})),
                action("frame.create", json!({"spread":2,"rect":[20,30,80,90]})),
                action("object.align", json!({"ids":["$0.id","$1.id"],"edge":"top"})),
            ],
        };
        assert!(map_plan(&cross_align, &before).is_err());

        let ungroup_ids = ProductionPlan {
            title: "Ungroup null result".into(),
            images: vec![],
            commands: vec![
                action("frame.create", json!({"spread":1,"rect":[20,30,80,90]})),
                action("frame.create", json!({"spread":1,"rect":[90,30,150,90]})),
                action("object.group", json!({"ids":["$0.id","$1.id"]})),
                action("object.ungroup", json!({"ids":["$2.id"]})),
                action("transform.set", json!({"ids":"$3.ids","x":25})),
            ],
        };
        assert!(map_plan(&ungroup_ids, &before).is_err());
    }

    #[test]
    fn object_effects_gradient_feather_endpoints_translate_per_facing_page_and_styles_reject_page_local_vectors() {
        for rtl in [false, true] {
            let mut session = Session::new();
            session.execute("file.new", &json!({"width": 400, "height": 500, "pages": 3, "facingPages": true})).unwrap();
            if rtl {
                session.execute("layout.documentSetup", &json!({"binding": "rightToLeft"})).unwrap();
            }
            let before = session.doc().unwrap().clone();
            let scope = crate::worker::BatchScope { start: 1, count: 2 };
            let plan = ProductionPlan {
                title: "Facing spread object.effects gradientFeather".into(),
                images: vec![],
                commands: vec![
                    action("frame.create", json!({"spread": 1, "content": "unassigned", "rect": [40, 60, 240, 160]})),
                    action(
                        "object.effects",
                        json!({"ids": ["$0.id"], "effects": {"gradientFeather": {"on": true, "start": 1.0, "end": 0.1, "from": [40, 60], "to": [240, 160]}}}),
                    ),
                    action("frame.create", json!({"spread": 2, "content": "unassigned", "rect": [50, 80, 250, 180]})),
                    action(
                        "object.effects",
                        json!({"ids": ["$2.id"], "effects": {"gradientFeather": {"on": true, "start": 0.9, "end": 0.0, "from": [50, 80], "to": [250, 180]}}}),
                    ),
                ],
            };
            let mapped = map_plan(&plan, &before).unwrap();
            // On 400pt-wide facing spread 1 (pages 1 & 2):
            // LTR: page 1 is left (x=0), page 2 is right (x=400).
            // RTL: page 1 is right (x=400), page 2 is left (x=0).
            let (p1_x, p2_x) = if rtl { (400.0, 0.0) } else { (0.0, 400.0) };
            assert_eq!(mapped.commands[1].params["effects"]["gradientFeather"]["from"], json!([40.0 + p1_x, 60]));
            assert_eq!(mapped.commands[1].params["effects"]["gradientFeather"]["to"], json!([240.0 + p1_x, 160]));
            assert_eq!(mapped.commands[3].params["effects"]["gradientFeather"]["from"], json!([50.0 + p2_x, 80]));
            assert_eq!(mapped.commands[3].params["effects"]["gradientFeather"]["to"], json!([250.0 + p2_x, 180]));

            let applied = session
                .execute("production.apply", &json!({"commands": mapped.commands, "outputMode": "native_only", "requirePreflight": true}))
                .unwrap();
            let after = session.doc().unwrap().clone();
            validate_mapped_result(&before, &after, scope).unwrap();
            // Both items must store feather endpoints in item-local path space matching feather_bounds()!
            for idx in [0usize, 2usize] {
                let id = doc::ItemId(applied["results"][idx]["id"].as_u64().unwrap());
                let it = after.doc.item(id).unwrap();
                let fb = it.feather_bounds();
                let vec = it.effects.gradient_feather.vector.unwrap();
                assert!(
                    (vec[0] - fb.x0).abs() < 1e-4 && (vec[1] - fb.y0).abs() < 1e-4 && (vec[2] - fb.x1).abs() < 1e-4 && (vec[3] - fb.y1).abs() < 1e-4,
                    "vec={vec:?} fb={fb:?}"
                );
            }
            // Also verify rotated item inverse-transforms spread-space endpoints via item_xf.inverse().
            let rot_plan = ProductionPlan {
                title: "Rotated facing item gradientFeather".into(),
                images: vec![],
                commands: vec![
                    action("frame.create", json!({"spread": 2, "content": "unassigned", "rect": [60, 100, 160, 200]})),
                    action("transform.rotate", json!({"ids": ["$0.id"], "angle": 90.0})),
                    action(
                        "object.effects",
                        json!({"ids": ["$0.id"], "effects": {"gradientFeather": {"on": true, "from": [60, 150], "to": [160, 150]}}}),
                    ),
                ],
            };
            let rot_mapped = map_plan(&rot_plan, &after).unwrap();
            let rot_applied = session
                .execute("production.apply", &json!({"commands": rot_mapped.commands, "outputMode": "native_only", "requirePreflight": true}))
                .unwrap();
            let rot_id = doc::ItemId(rot_applied["results"][0]["id"].as_u64().unwrap());
            let rot_item = session.doc().unwrap().doc.item(rot_id).unwrap();
            let rot_vec = rot_item.effects.gradient_feather.vector.unwrap();
            let p_start = rot_item.xf * designcraft_geom::Point::new(rot_vec[0], rot_vec[1]);
            let p_end = rot_item.xf * designcraft_geom::Point::new(rot_vec[2], rot_vec[3]);
            assert!((p_start.x - (60.0 + p2_x)).abs() < 1e-4 && (p_start.y - 150.0).abs() < 1e-4, "p_start={p_start:?}");
            assert!((p_end.x - (160.0 + p2_x)).abs() < 1e-4 && (p_end.y - 150.0).abs() < 1e-4, "p_end={p_end:?}");

            // Cross-page object.effects gradientFeather and style.object.create endpoints must reject cleanly.
            let cross = ProductionPlan {
                title: "Cross-page effects".into(),
                images: vec![],
                commands: vec![
                    action("frame.create", json!({"spread": 1, "rect": [40, 60, 140, 120]})),
                    action("frame.create", json!({"spread": 2, "rect": [40, 60, 140, 120]})),
                    action(
                        "object.effects",
                        json!({"ids": ["$0.id", "$1.id"], "effects": {"gradientFeather": {"on": true, "from": [40, 60], "to": [140, 120]}}}),
                    ),
                ],
            };
            assert!(map_plan(&cross, &before).is_err());

            let style_vec = ProductionPlan {
                title: "Style page-local feather".into(),
                images: vec![],
                commands: vec![action(
                    "style.object.create",
                    json!({"name": "CardFade", "effects": {"gradientFeather": {"on": true, "from": [40, 60], "to": [140, 120]}}}),
                )],
            };
            assert!(map_plan(&style_vec, &before).is_err());

            for (cmd, params) in [
                ("style.object.create", json!({"name": "TopFade", "gradientFeather": {"on": true, "from": [40, 60], "to": [140, 120]}})),
                (
                    "style.object.edit",
                    json!({"name": "[Basic Graphics Frame]", "effects": {"gradientFeather": {"on": true, "from": [40, 60], "to": [140, 120]}}}),
                ),
                ("style.object.edit", json!({"name": "[Basic Graphics Frame]", "gradientFeather": {"on": true, "vector": [40, 60, 140, 120]}})),
            ] {
                let bad_plan = ProductionPlan { title: "Bad style feather".into(), images: vec![], commands: vec![action(cmd, params.clone())] };
                assert!(map_plan(&bad_plan, &before).is_err(), "{cmd} should fail in map_plan");
                assert!(session.execute(cmd, &params).is_err(), "{cmd} should fail in direct Session::execute");
            }
        }
    }

    #[test]
    fn nested_rotated_sheared_group_gradient_feather_and_unit_zero_end_scale_and_export_cleanly() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"width": 400, "height": 500, "pages": 3, "facingPages": true})).unwrap();
        session.execute("layout.documentSetup", &json!({"binding": "rightToLeft"})).unwrap();
        let before = session.doc().unwrap().clone();

        // 1. `production.apply` with `object.gradientFeather` `start: 1.0, end: 0.0` must scale `start`
        // to 100% (1.0), NOT leave it unscaled so `paths.rs` divides by 100 to 0.01 (99% invisible).
        let unit_plan = ProductionPlan {
            title: "Unit 1.0 -> 0.0 gradientFeather".into(),
            images: vec![],
            commands: vec![
                action("frame.create", json!({"spread": 1, "content": "unassigned", "rect": [40, 40, 240, 140]})),
                action("object.fill", json!({"ids": ["$0.id"], "swatch": "[Black]"})),
                action("object.gradientFeather", json!({"ids": ["$0.id"], "on": true, "start": 1.0, "end": 0.0, "from": [40, 90], "to": [240, 90]})),
            ],
        };
        let unit_mapped = map_plan(&unit_plan, &before).unwrap();
        let unit_applied = session
            .execute("production.apply", &json!({"commands": unit_mapped.commands, "outputMode": "native_only", "requirePreflight": true}))
            .unwrap();
        let unit_id = doc::ItemId(unit_applied["results"][0]["id"].as_u64().unwrap());
        let unit_gf = &session.doc().unwrap().doc.item(unit_id).unwrap().effects.gradient_feather;
        assert!((unit_gf.start - 1.0).abs() < 1e-5 && unit_gf.end.abs() < 1e-5, "unit_gf={unit_gf:?}");

        // 2. Nested rotated + sheared group hierarchy on RTL facing page 1 (spread x offset = +400):
        // build inner + outer groups, rotate + shear the outer group, then apply `object.effects`
        // `gradientFeather` via `map_plan` on the leaf frame and verify round-trip transform,
        // rendered pixel alpha gradient, and PDF export.
        let leaf_a = session.execute("frame.create", &json!({"spread": 1, "content": "unassigned", "rect": [460, 180, 620, 280]})).unwrap()["id"]
            .as_u64()
            .unwrap();
        session.execute("object.fill", &json!({"ids": [leaf_a], "swatch": "[Black]"})).unwrap();
        let leaf_b = session.execute("frame.create", &json!({"spread": 1, "content": "unassigned", "rect": [460, 300, 520, 340]})).unwrap()["id"]
            .as_u64()
            .unwrap();
        let inner_g = session.execute("object.group", &json!({"ids": [leaf_a, leaf_b]})).unwrap()["id"].as_u64().unwrap();
        session.execute("transform.rotate", &json!({"ids": [inner_g], "angle": 25.0})).unwrap();
        let helper = session.execute("frame.create", &json!({"spread": 1, "content": "unassigned", "rect": [640, 180, 680, 220]})).unwrap()["id"]
            .as_u64()
            .unwrap();
        let outer_g = session.execute("object.group", &json!({"ids": [inner_g, helper]})).unwrap()["id"].as_u64().unwrap();
        session.execute("transform.set", &json!({"ids": [outer_g], "rotation": -15.0, "shear": 12.0})).unwrap();

        let grouped_state = session.doc().unwrap().clone();
        let loc = grouped_state.doc.find(doc::ItemId(leaf_a)).unwrap();
        assert_eq!(loc.path.len(), 3, "leaf must be nested two groups deep");
        let full_xf = grouped_state.doc.parent_xf(&loc) * grouped_state.doc.item(doc::ItemId(leaf_a)).unwrap().xf;
        let center_spread = full_xf * grouped_state.doc.item(doc::ItemId(leaf_a)).unwrap().inner_bounds().center();
        // Convert spread x back to page-local draft x on RTL page 1 (page.x == 400.0):
        let draft_x0 = center_spread.x - 400.0 - 40.0;
        let draft_x1 = center_spread.x - 400.0 + 40.0;
        let draft_y = center_spread.y;

        let nested_plan = ProductionPlan {
            title: "Nested rotated+sheared group leaf feather".into(),
            images: vec![],
            commands: vec![action(
                "object.effects",
                json!({
                    "ids": [leaf_a],
                    "effects": {"gradientFeather": {"on": true, "start": 1.0, "end": 0.0, "from": [draft_x0, draft_y], "to": [draft_x1, draft_y]}}
                }),
            )],
        };
        let nested_mapped = map_plan(&nested_plan, &grouped_state).unwrap();
        session
            .execute("production.apply", &json!({"commands": nested_mapped.commands, "outputMode": "native_only", "requirePreflight": false}))
            .unwrap();

        let final_state = session.doc().unwrap();
        let leaf_item = final_state.doc.item(doc::ItemId(leaf_a)).unwrap();
        let vec = leaf_item.effects.gradient_feather.vector.unwrap();
        let world_start = full_xf * designcraft_geom::Point::new(vec[0], vec[1]);
        let world_end = full_xf * designcraft_geom::Point::new(vec[2], vec[3]);
        assert!((world_start.x - (draft_x0 + 400.0)).abs() < 1e-4 && (world_start.y - draft_y).abs() < 1e-4, "world_start={world_start:?}");
        assert!((world_end.x - (draft_x1 + 400.0)).abs() < 1e-4 && (world_end.y - draft_y).abs() < 1e-4, "world_end={world_end:?}");

        // Render RTL page 1 (absolute page index 1) and verify dark pixels at start fade toward white paper at end.
        let mut renderer = designcraft_render::Renderer::new();
        let rendered = renderer.render_page(&final_state.doc, &session.cache, 1, 1.0, false, &designcraft_render::RenderOptions::default()).unwrap();
        let px_at = |x: f64, y: f64| -> [u8; 4] {
            let x = (x.round() as u32).clamp(0, rendered.width - 1);
            let y = (y.round() as u32).clamp(0, rendered.height - 1);
            let idx = ((y * rendered.width + x) * 4) as usize;
            rendered.pixels[idx..idx + 4].try_into().unwrap()
        };
        let px_start = px_at(draft_x0, draft_y);
        let px_end = px_at(draft_x1, draft_y);
        assert!(px_start[0] < px_end[0], "opaque start pixel {px_start:?} must be darker than feathered end pixel {px_end:?}");

        // Also verify PDF export succeeds cleanly with nested transformed gradientFeather.
        let dir = tempfile::tempdir().unwrap();
        let pdf_path = dir.path().join("nested-facing-feather.pdf");
        session.execute("file.exportPdf", &json!({"path": pdf_path})).unwrap();
        assert!(std::fs::metadata(&pdf_path).unwrap().len() > 500);
    }

    #[test]
    fn ai28_mirror_facing_footer_outer_flips_folio_and_bar_away_from_spine_in_rtl_and_ltr() {
        let footer_cmds = |abs_page: u64, x0: f64, x1: f64, align: &str, bar_x0: f64, bar_x1: f64| -> Vec<designcraft_ai_types::EngineAction> {
            vec![
                action(
                    "path.create",
                    json!({"spread": abs_page, "closed": true, "anchors": [{"p": [bar_x0, 790.0]}, {"p": [bar_x1, 790.0]}, {"p": [bar_x1, 804.0]}, {"p": [bar_x0, 804.0]}]}),
                ),
                action("object.fill", json!({"ids": ["$0.id"], "swatch": "[Black]"})),
                action("object.label", json!({"ids": ["$0.id"], "label": "part.ui.footer.bar"})),
                action("frame.create", json!({"spread": abs_page, "rect": [x0, 790.0, x1, 804.0], "content": "text", "text": "2"})),
                action("text.select", json!({"story": "$3.story", "anchor": 0, "focus": 1})),
                action("type.para", json!({"attrs": {"align": align}})),
                action("object.label", json!({"ids": ["$3.id"], "label": "part.ui.footer.page"})),
            ]
        };

        // 1. RTL facing document (400pt wide pages):
        // Page 0 = Cover (Single, Right), Spread 1 holds Page 1 (Right, x=400..800, spine on left edge of page)
        // and Page 2 (Left, x=0..400, spine on right edge of page).
        let mut rtl_session = Session::new();
        rtl_session
            .execute("file.new", &json!({"width": 400.0, "height": 820.0, "pages": 3, "facingPages": true, "binding": "rightToLeft"}))
            .unwrap();
        let rtl_state = rtl_session.doc().unwrap().clone();
        // Put left-side folio [24..64] on BOTH page 1 (Right page -> spine is on its left!) and page 2 (Left page -> outer is on its left!).
        let mut rtl_cmds = footer_cmds(1, 24.0, 64.0, "left", 20.0, 90.0);
        rtl_cmds.extend(footer_cmds(2, 24.0, 64.0, "left", 20.0, 90.0));
        mirror_facing_footer_outer(&mut rtl_cmds, &rtl_state).unwrap();
        // Page 1 (Right page): folio was near spine (x=24..64 < 200) -> mirrored to outer right [336..376], align="right", bar mirrored [380..310].
        assert_eq!(rtl_cmds[3].params["rect"], json!([336.0, 790.0, 376.0, 804.0]));
        assert_eq!(rtl_cmds[5].params["attrs"]["align"], json!("right"));
        assert_eq!(rtl_cmds[0].params["anchors"][0]["p"], json!([380.0, 790.0]));
        // Page 2 (Left page): folio at [24..64] is ALREADY on the outer left edge -> untouched!
        assert_eq!(rtl_cmds[10].params["rect"], json!([24.0, 790.0, 64.0, 804.0]));
        assert_eq!(rtl_cmds[12].params["attrs"]["align"], json!("left"));
        assert_eq!(rtl_cmds[7].params["anchors"][0]["p"], json!([20.0, 790.0]));

        // 2. LTR facing document (400pt wide pages):
        // Spread 1 holds Page 1 (Left, x=0..400, spine on right edge of page) and Page 2 (Right, x=400..800, spine on left edge of page).
        let mut ltr_session = Session::new();
        ltr_session
            .execute("file.new", &json!({"width": 400.0, "height": 820.0, "pages": 3, "facingPages": true, "binding": "leftToRight"}))
            .unwrap();
        let ltr_state = ltr_session.doc().unwrap().clone();
        // Put right-side folio [336..376] on BOTH page 1 (Left page -> spine is on its right!) and page 2 (Right page -> outer is on its right!).
        let mut ltr_cmds = footer_cmds(1, 336.0, 376.0, "right", 310.0, 380.0);
        ltr_cmds.extend(footer_cmds(2, 336.0, 376.0, "right", 310.0, 380.0));
        mirror_facing_footer_outer(&mut ltr_cmds, &ltr_state).unwrap();
        // Page 1 (Left page): folio was near spine (x=336..376 > 200) -> mirrored to outer left [24..64], align="left", bar mirrored [90..20].
        assert_eq!(ltr_cmds[3].params["rect"], json!([24.0, 790.0, 64.0, 804.0]));
        assert_eq!(ltr_cmds[5].params["attrs"]["align"], json!("left"));
        assert_eq!(ltr_cmds[0].params["anchors"][0]["p"], json!([90.0, 790.0]));
        // Page 2 (Right page): folio at [336..376] is ALREADY on the outer right edge -> untouched!
        assert_eq!(ltr_cmds[10].params["rect"], json!([336.0, 790.0, 376.0, 804.0]));
        assert_eq!(ltr_cmds[12].params["attrs"]["align"], json!("right"));
        assert_eq!(ltr_cmds[7].params["anchors"][0]["p"], json!([310.0, 790.0]));
    }
}
