//! Compile validated JSON page-slot copy and illustration slots into native engine actions.
use super::counting::{
    NumberTriplet, bindable_counting_family, bound_counting_template, contains_image_quantity_claim, counting_family, number_triplet,
};
use super::curriculum::{CurriculumSubject, detect_curriculum_subject, extract_unit_number_from_text, seed_unit_from_preceding_pages};
use super::{MAX_ACTIONS, MAX_IMAGES, MAX_JSON, MAX_SLOT_CHARS, action_rect_array, base_char_count, profile, rect_approx_eq, scope_end};
use crate::worker::BatchScope;
use designcraft_ai_skills::SkillSnapshot;
use designcraft_ai_types::design::PageFamily;
use designcraft_ai_types::protocol::{ImagePlacement, ProductionPlan};
use designcraft_ai_types::{EngineAction, OutputMode};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentPlan {
    title: String,
    pages: Vec<ContentPage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentPage {
    page: u32,
    family: String,
    #[serde(deserialize_with = "unique_text")]
    text: BTreeMap<String, String>,
    #[serde(default)]
    illustrations: Vec<ContentImage>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContentImage {
    slot: String,
    prompt: String,
}

fn unique_text<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error> {
    struct Unique;
    impl<'de> serde::de::Visitor<'de> for Unique {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a text-slot object with unique keys")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, String>()? {
                if result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("Duplicate text slot"));
                }
            }
            Ok(result)
        }
    }
    deserializer.deserialize_map(Unique)
}

fn bounded_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && !text.contains('\0') && text.chars().count() <= max
}

/// Find the slot's last authored alignment even when formatting is separated by
/// labels, selections, other frames, or unrelated paragraph formatting.
fn authored_center_alignment(template: &[EngineAction], target: usize) -> Option<&str> {
    let frame = format!("${target}.id");
    let story = format!("${target}.story");
    let mut selected = false;
    let mut alignment = None;
    for (index, action) in template.iter().enumerate() {
        match action.command.as_str() {
            "frame.create" | "line.create" | "path.create" | "object.group" => selected = index == target,
            "selection.set" => {
                let contains = action.params.get("ids").and_then(Value::as_array).is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(&frame)));
                selected = contains || (selected && action.params.get("add").and_then(Value::as_bool) == Some(true));
            }
            "text.select" => selected = action.params.get("story").and_then(Value::as_str) == Some(&story),
            "type.para" if selected => {
                if let Some(align) = action.params.get("attrs").and_then(|attrs| attrs.get("align")) {
                    alignment = align.as_str();
                }
            }
            _ => {}
        }
    }
    alignment.filter(|align| matches!(*align, "center" | "centerJustified"))
}

#[derive(Clone, Copy, Default)]
pub(crate) struct TemplateCompileContext<'a> {
    pub base: Option<&'a designcraft_engine::DocState>,
    pub facing_pages: bool,
    pub total_pages: u32,
    pub subject: Option<CurriculumSubject>,
    pub visual_density: designcraft_ai_types::VisualDensityMode,
    pub soft_fit_slots: bool,
    pub explicit_script: bool,
}

#[cfg(test)]
pub(crate) fn compile(text: &str, skill: &SkillSnapshot, scope: BatchScope, mode: OutputMode) -> Result<ProductionPlan, String> {
    compile_inner(text, skill, scope, mode, true, None)
}

#[cfg(test)]
pub(crate) fn compile_for_brief(
    text: &str,
    skill: &SkillSnapshot,
    scope: BatchScope,
    mode: OutputMode,
    brief: &str,
) -> Result<ProductionPlan, String> {
    let subject = detect_curriculum_subject(brief);
    compile_for_brief_with_context(
        text,
        skill,
        scope,
        mode,
        brief,
        TemplateCompileContext { base: None, facing_pages: false, total_pages: 0, subject: Some(subject), ..Default::default() },
    )
}

pub(crate) fn compile_for_brief_with_context(
    text: &str,
    skill: &SkillSnapshot,
    scope: BatchScope,
    mode: OutputMode,
    brief: &str,
    mut ctx: TemplateCompileContext<'_>,
) -> Result<ProductionPlan, String> {
    let subject = ctx.subject.unwrap_or_else(|| detect_curriculum_subject(brief));
    ctx.subject = Some(subject);
    ctx.explicit_script = crate::script_contract::parse(brief)?.is_some();
    let effective_mode = if ctx.visual_density == designcraft_ai_types::VisualDensityMode::PureNativeVector { OutputMode::NativeOnly } else { mode };
    let mut plan = compile_inner(text, skill, scope, effective_mode, true, Some(ctx))?;
    if subject != CurriculumSubject::Mathematics
        && plan.commands.iter().any(|action| {
            action.command == "object.label" && action.params.get("label").and_then(Value::as_str) == Some("group.cpa.pictorial-tenframe.bound")
        })
    {
        return Err("The bound Cambridge counting family requires an explicit mathematics brief".into());
    }
    crate::script_contract::validate_plan(brief, scope, &plan)?;
    crate::script_contract::anchor_images(brief, scope, &mut plan)?;
    Ok(plan)
}

pub(super) fn compile_inner(
    text: &str,
    skill: &SkillSnapshot,
    scope: BatchScope,
    mode: OutputMode,
    verify_math: bool,
    ctx: Option<TemplateCompileContext<'_>>,
) -> Result<ProductionPlan, String> {
    if text.len() > MAX_JSON {
        return Err("Template content exceeds 1 MiB".into());
    }
    let profile = profile(skill)?;
    let end = scope_end(scope)?;
    let mut de = serde_json::Deserializer::from_str(text);
    let content: ContentPlan = serde_path_to_error::deserialize(&mut de).map_err(|e| format!("Invalid template content: {e}"))?;
    de.end().map_err(|e| format!("Invalid template content: {e}"))?;
    if !bounded_text(&content.title, 256) || u64::try_from(content.pages.len()).map_err(|_| "Too many pages")? != u64::from(scope.count) {
        return Err("Template content title or page count is invalid".into());
    }
    let mut pages = BTreeMap::new();
    for page in content.pages {
        if page.page < scope.start || page.page >= end || pages.insert(page.page, page).is_some() {
            return Err("Template content has duplicate or out-of-scope pages".into());
        }
    }
    let allowed: BTreeSet<String> = crate::worker::batch_catalog(designcraft_engine::cmd::production::catalog())
        .get("commands")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("command").and_then(Value::as_str))
        .filter(|name| *name != "file.place")
        .map(str::to_owned)
        .collect();
    let mut plan = ProductionPlan { title: content.title, commands: Vec::new(), images: Vec::new() };
    let mut active_unit = ctx.and_then(|c| c.base).and_then(|base| seed_unit_from_preceding_pages(base, scope.start));
    let use_relational_master = ctx.and_then(|c| c.base).is_some_and(super::relational::has_relational_master_footer);
    let has_unit_heading_style = ctx.and_then(|c| c.base).is_some_and(super::relational::base_has_unit_heading_style);
    let soft_fit = ctx.is_some_and(|c| c.soft_fit_slots && !c.explicit_script);
    let pure_native_vector = ctx.is_some_and(|c| c.visual_density == designcraft_ai_types::VisualDensityMode::PureNativeVector);
    for page_number in scope.start..end {
        let mut page = pages.remove(&page_number).ok_or("Template content is missing a requested page")?;
        let family = profile.page_families.iter().find(|family| family.id == page.family).ok_or("Unknown content page family")?;
        let raw_template = skill.templates.get(&family.template).ok_or("Missing native page template")?;
        prepare_and_validate_page_slots(&mut page, page_number, family, raw_template, mode, (soft_fit, pure_native_vector))?;
        let template = raw_template;
        let bindable = counting_family(family, template) && bindable_counting_family(family, template);
        let quantities = if bindable {
            Some(if verify_math {
                number_triplet(&page.text).map_err(|error| format!("Page {} ({}): {error}", u64::from(page_number) + 1, family.id))?
            } else {
                // Private capacity-only worst case, never published or inferred
                // from sample artwork. Every real page requires authored values.
                NumberTriplet { left: 5, right: 5, total: 10 }
            })
        } else {
            None
        };
        if verify_math && counting_family(family, template) && !bindable {
            return Err(format!(
                "Page {} ({}): fixed counting-frame geometry and tokens are not bound to lesson quantities; use a structured activity in adaptive native planning",
                u64::from(page_number) + 1,
                family.id
            ));
        }
        let bound_template = quantities.map(|values| bound_counting_template(template, values)).transpose()?;
        let template = bound_template.as_deref().unwrap_or(template);
        let (filtered_tpl, slot_cmds) = super::relational::prepare_template_for_relational_master(template, family, ctx.and_then(|c| c.base))?;
        let template = filtered_tpl.as_deref().unwrap_or(template);
        let has_ui_chrome = template
            .iter()
            .any(|a| a.command == "object.label" && a.params.get("label").and_then(Value::as_str).is_some_and(|lbl| lbl.starts_with("part.ui.")));
        let arabic_slots = family
            .text_slots
            .iter()
            .filter(|slot| page.text.get(&slot.id).is_some_and(|t| t.chars().any(|c| ('\u{0600}'..='\u{06ff}').contains(&c) && c.is_alphabetic())))
            .count();
        let mirror_rtl = has_ui_chrome && arabic_slots.saturating_mul(2) > family.text_slots.len();
        let page_w = profile.page_mm[0] * 72.0 / 25.4;
        let explicit_page_unit =
            ["header-tag", "title-main"].into_iter().filter_map(|slot_id| page.text.get(slot_id)).find_map(|txt| extract_unit_number_from_text(txt));
        if let Some(u) = explicit_page_unit {
            active_unit = Some(u);
        }
        let resolved_unit_badge = active_unit.or_else(|| {
            ctx.map(|c| {
                let _ = c.total_pages;
                if page_number == 0 || family.id == "international-scope-sequence" {
                    1
                } else {
                    page_number.saturating_sub(1).checked_div(8).unwrap_or(0).saturating_add(1)
                }
            })
        });
        let even_facing_rtl = mirror_rtl && ctx.is_some_and(|c| c.facing_pages) && page_number % 2 == 1;
        let (offset, section_marker_for_page) = emit_template_page_actions(
            &mut plan,
            template,
            &allowed,
            &slot_cmds,
            (&page, family, mode),
            (page_number, resolved_unit_badge, mirror_rtl, even_facing_rtl, page_w),
        )?;
        apply_compiled_slot_directions(&mut plan, template, family, &page.text, &slot_cmds, offset, mirror_rtl)?;
        finalize_template_page_chrome(
            &mut plan,
            template,
            family,
            &slot_cmds,
            offset,
            (page_number, section_marker_for_page, use_relational_master, has_unit_heading_style),
        )?;
        append_images(&mut plan, &skill.skill_id, &page, family, mode, quantities, &profile.illustration, mirror_rtl.then_some(page_w), ctx)?;
    }
    crate::worker::validate_batch_plan(&plan, scope)?;
    Ok(plan)
}

fn prepare_and_validate_page_slots(
    page: &mut ContentPage,
    page_number: u32,
    family: &designcraft_ai_types::PageFamily,
    raw_template: &[EngineAction],
    mode: OutputMode,
    flags: (bool, bool),
) -> Result<(), String> {
    let (soft_fit, pure_native_vector) = flags;
    if soft_fit {
        page.text.retain(|key, _| family.text_slots.iter().any(|slot| &slot.id == key));
        let authored_non_empty = family.text_slots.iter().filter(|slot| page.text.get(&slot.id).is_some_and(|val| !val.trim().is_empty())).count();
        if authored_non_empty > 0 && family.text_slots.len() > 1 {
            for slot in &family.text_slots {
                if !page.text.contains_key(&slot.id) {
                    let fallback = raw_template
                        .get(slot.command)
                        .and_then(|act| act.params.get("text"))
                        .and_then(Value::as_str)
                        .map(|t| t.replace("{{page}}", &u64::from(page_number).saturating_add(1).to_string()))
                        .filter(|t| !t.trim().is_empty())
                        .unwrap_or_else(|| "—".to_owned());
                    page.text.insert(slot.id.clone(), fallback);
                }
            }
        }
        if mode == OutputMode::NativeOnly {
            page.illustrations.clear();
        }
    } else if pure_native_vector && mode == OutputMode::NativeOnly {
        page.illustrations.clear();
    }
    for slot in &family.text_slots {
        let location = format!("Page {} ({}) / text slot {}", u64::from(page_number) + 1, family.id, slot.id);
        let value = page.text.get(&slot.id).ok_or_else(|| format!("{location} is missing"))?;
        if value.trim().is_empty() {
            return Err(format!("{location} is empty"));
        }
        if value.contains('\0') {
            return Err(format!("{location} contains a null character"));
        }
        let base_count = base_char_count(value);
        let raw_count = value.chars().count();
        if base_count > MAX_SLOT_CHARS || raw_count > MAX_SLOT_CHARS.saturating_mul(3) {
            return Err(format!("{location} contains {base_count} characters; the safety limit is {MAX_SLOT_CHARS}"));
        }
    }
    if page.text.len() != family.text_slots.len() {
        return Err(format!("Page {} ({}): content text includes undeclared slot IDs", u64::from(page_number) + 1, family.id));
    }
    Ok(())
}

fn emit_template_page_actions(
    plan: &mut ProductionPlan,
    template: &[EngineAction],
    allowed: &BTreeSet<String>,
    slot_cmds: &BTreeMap<String, usize>,
    page_ctx: (&ContentPage, &designcraft_ai_types::PageFamily, OutputMode),
    rtl_ctx: (u32, Option<u32>, bool, bool, f64),
) -> Result<(usize, Option<String>), String> {
    let (page, family, mode) = page_ctx;
    let (page_number, resolved_unit_badge, mirror_rtl, even_facing_rtl, page_w) = rtl_ctx;
    let offset = plan.commands.len();
    if offset.checked_add(template.len()).is_none_or(|count| count > MAX_ACTIONS) {
        return Err("Compiled template exceeds 512 commands".into());
    }
    let mut section_marker_for_page: Option<String> = None;
    for (index, action) in template.iter().enumerate() {
        if !allowed.contains(&action.command) {
            return Err(format!("Template command {} is not page-isolated", action.command));
        }
        let mut budget = 65_536;
        validate_tree(&action.params, 0, &mut budget)?;
        let orig_rect = action_rect_array(action);
        let frame_label = template
            .get(index.saturating_add(3))
            .filter(|next| next.command == "object.label")
            .and_then(|next| next.params.get("label"))
            .and_then(Value::as_str);
        let path_label = template
            .get(index.saturating_add(2))
            .filter(|next| next.command == "object.label")
            .and_then(|next| next.params.get("label"))
            .and_then(Value::as_str);
        let linked_caption_txt = if action.command == "object.altText" {
            super::relational::linked_caption_slot_text(template, index, slot_cmds, &page.text)
        } else {
            None
        };
        let mut action: EngineAction = action.clone();
        let params = action.params.as_object_mut().ok_or("Native template parameters must be an object")?;
        for (key, value) in params.iter_mut() {
            if key == "text" && action.command == "frame.create" {
                let raw = value.as_str().ok_or("Native frame text must be a string")?;
                let supplied = slot_cmds.iter().find(|(_, cmd_idx)| **cmd_idx == index).and_then(|(slot_id, _)| page.text.get(slot_id));
                let text = if let Some(supplied) = supplied {
                    supplied.clone()
                } else if frame_label == Some("part.ui.header.unit-num")
                    && let Some(unit_no) = resolved_unit_badge
                {
                    section_marker_for_page = Some(unit_no.to_string());
                    "\u{E001}".to_string()
                } else if frame_label == Some("part.listen.sample-tick") {
                    " ".to_string()
                } else {
                    let resolved = raw.replace("{{page}}", &u64::from(page_number).saturating_add(1).to_string());
                    if matches!(frame_label, Some("part.skills.listen-checks" | "part.lab.steps-txt")) {
                        resolved.replace("[v]", "( )")
                    } else if mirror_rtl {
                        resolved.replace("[v]", "✓")
                    } else {
                        resolved
                    }
                };
                *value = Value::String(text.replace('$', "$$"));
            } else if key == "text"
                && action.command == "object.altText"
                && let Some(caption_txt) = linked_caption_txt
            {
                *value = Value::String(caption_txt.replace('$', "$$"));
            } else {
                rebase(value, index, offset)?;
            }
        }
        if action.command == "frame.create"
            && params.get("content").and_then(Value::as_str) == Some("graphic")
            && let Some(r) = orig_rect
            && let Some(slot) = family.image_slots.iter().find(|slot| rect_approx_eq(r, slot.rect))
            && (mode == OutputMode::NativeOnly || !page.illustrations.iter().any(|img| img.slot == slot.id))
        {
            params.insert("content".into(), json!("unassigned"));
        }
        if mirror_rtl {
            apply_rtl_template_mirroring(&action.command, params, template, index, (even_facing_rtl, frame_label, path_label, page_w));
        }
        if matches!(action.command.as_str(), "frame.create" | "line.create" | "path.create") || params.contains_key("spread") {
            params.insert("spread".into(), json!(page_number));
        }
        plan.commands.push(action);
    }
    Ok((offset, section_marker_for_page))
}

fn apply_rtl_template_mirroring(
    command: &str,
    params: &mut Map<String, Value>,
    template: &[EngineAction],
    index: usize,
    rtl_info: (bool, Option<&str>, Option<&str>, f64),
) {
    let (even_facing_rtl, frame_label, path_label, page_w) = rtl_info;
    let keep_outer_right_footer = even_facing_rtl && (frame_label == Some("part.ui.footer.page") || path_label == Some("part.ui.footer.bar"));
    if !keep_outer_right_footer {
        mirror_template_action_x(command, params, page_w);
    }
    if command == "object.stroke"
        && template.get(index.saturating_add(1)).is_some_and(|next| {
            next.command == "object.label" && next.params.get("label").and_then(Value::as_str).is_some_and(|lbl| lbl.starts_with("part.wb.cw.col-"))
        })
    {
        params.insert("weight".into(), json!(0.0));
        params.insert("swatch".into(), json!("[None]"));
    }
    if command == "type.para"
        && template
            .get(index.saturating_add(1))
            .is_some_and(|next| next.command == "object.label" && next.params.get("label").and_then(Value::as_str) == Some("part.ui.footer.page"))
        && let Some(attrs) = params.get_mut("attrs").and_then(Value::as_object_mut)
    {
        attrs.insert("direction".into(), json!("rightToLeft"));
        attrs.insert("align".into(), json!(if even_facing_rtl { "right" } else { "left" }));
    }
}

fn apply_compiled_slot_directions(
    plan: &mut ProductionPlan,
    template: &[EngineAction],
    family: &designcraft_ai_types::PageFamily,
    page_text: &BTreeMap<String, String>,
    slot_cmds: &BTreeMap<String, usize>,
    offset: usize,
    mirror_rtl: bool,
) -> Result<(), String> {
    // Adapt language/direction without silently shrinking the approved typography.
    // Actual shaping and overset are checked by the native engine before images are bought.
    for slot in &family.text_slots {
        let text = page_text.get(&slot.id).ok_or("Missing validated text slot")?;
        let has_arabic_script = text.chars().any(|c| {
            ('\u{0600}'..='\u{06ff}').contains(&c)
                && (c.is_alphabetic() || ('\u{0660}'..='\u{0669}').contains(&c) || ('\u{06f0}'..='\u{06f9}').contains(&c))
        });
        let has_latin_alpha = text.chars().any(|c| c.is_ascii_alphabetic());
        let has_arabic = has_arabic_script || (mirror_rtl && !has_latin_alpha && text.chars().any(|c| c.is_ascii_digit()));
        let latin_only = !has_arabic && text.chars().any(|c| c.is_ascii_alphanumeric()) && !text.chars().any(|c| c.is_alphabetic() && !c.is_ascii());
        let slot_cmd = *slot_cmds.get(&slot.id).ok_or("Missing mapped text slot command")?;
        let target = offset.checked_add(slot_cmd).ok_or("Text slot target overflow")?;
        let char_idx = target.checked_add(1);
        let para_idx = target.checked_add(2);
        let inplace_pair = char_idx.is_some_and(|i| plan.commands.get(i).is_some_and(|a| a.command == "type.char"))
            && para_idx.is_some_and(|i| plan.commands.get(i).is_some_and(|a| a.command == "type.para"));
        if inplace_pair {
            if let Some(c_idx) = char_idx
                && let Some(attrs) = plan.commands.get_mut(c_idx).and_then(|a| a.params.get_mut("attrs")).and_then(Value::as_object_mut)
            {
                if has_arabic {
                    attrs.insert("language".into(), json!("ar"));
                } else if latin_only {
                    attrs.insert("language".into(), json!("en"));
                }
            }
            if let Some(p_idx) = para_idx
                && let Some(attrs) = plan.commands.get_mut(p_idx).and_then(|a| a.params.get_mut("attrs")).and_then(Value::as_object_mut)
            {
                let is_center = matches!(attrs.get("align").and_then(Value::as_str), Some("center" | "centerJustified"));
                if has_arabic {
                    attrs.insert("direction".into(), json!("rightToLeft"));
                    if !is_center {
                        attrs.insert("align".into(), json!("right"));
                    }
                } else if latin_only {
                    attrs.insert("direction".into(), json!("leftToRight"));
                    if !is_center {
                        attrs.insert("align".into(), json!("left"));
                    }
                }
            }
        } else if has_arabic || latin_only {
            if plan.commands.len().checked_add(3).is_none_or(|count| count > MAX_ACTIONS) {
                return Err("Compiled text directions exceed 512 commands".into());
            }
            let (language, direction, default_align) = if has_arabic { ("ar", "rightToLeft", "right") } else { ("en", "leftToRight", "left") };
            let align = authored_center_alignment(template, slot_cmd).unwrap_or(default_align);
            plan.commands.push(EngineAction { command: "selection.set".into(), params: json!({"ids":[format!("${target}.id")]}) });
            plan.commands.push(EngineAction { command: "type.char".into(), params: json!({"attrs":{"language":language}}) });
            plan.commands.push(EngineAction { command: "type.para".into(), params: json!({"attrs":{"direction":direction,"align":align}}) });
        }
    }
    Ok(())
}

fn finalize_template_page_chrome(
    plan: &mut ProductionPlan,
    template: &[EngineAction],
    family: &designcraft_ai_types::PageFamily,
    slot_cmds: &BTreeMap<String, usize>,
    offset: usize,
    chrome_opts: (u32, Option<String>, bool, bool),
) -> Result<(), String> {
    let (page_number, section_marker_for_page, use_relational_master, has_unit_heading_style) = chrome_opts;
    let wb_cap_ids: Vec<String> = template
        .iter()
        .enumerate()
        .filter_map(|(idx, act)| {
            if act.command != "frame.create" {
                return None;
            }
            let lbl = template
                .get(idx.saturating_add(3))
                .filter(|next| next.command == "object.label")
                .and_then(|next| next.params.get("label"))
                .and_then(Value::as_str)?;
            matches!(lbl, "part.wb.cap-1" | "part.wb.cap-2").then(|| format!("${}.id", offset.saturating_add(idx)))
        })
        .collect();
    if !wb_cap_ids.is_empty() && plan.commands.len() < MAX_ACTIONS {
        plan.commands.push(EngineAction { command: "object.fill".into(), params: json!({"ids": wb_cap_ids, "swatch": "[Paper]", "tint": 0.92}) });
    }
    if let Some(marker) = section_marker_for_page {
        if plan.commands.len() >= MAX_ACTIONS {
            return Err("Compiled template exceeds 512 commands".into());
        }
        plan.commands.push(EngineAction { command: "layout.section".into(), params: json!({"page": u64::from(page_number) + 1, "marker": marker}) });
    }
    if use_relational_master {
        if family.id.contains("cover") {
            if plan.commands.len() >= MAX_ACTIONS {
                return Err("Compiled template exceeds 512 commands".into());
            }
            plan.commands.push(EngineAction { command: "layout.pages.applyParent".into(), params: json!({"pages": [page_number], "parent": null}) });
        } else if has_unit_heading_style && let Some(&title_cmd) = slot_cmds.get("title-main") {
            let title_target = offset.checked_add(title_cmd).ok_or("Title slot target overflow")?;
            if plan.commands.len().checked_add(2).is_none_or(|count| count > MAX_ACTIONS) {
                return Err("Compiled template exceeds 512 commands".into());
            }
            plan.commands.push(EngineAction {
                command: "text.select".into(),
                params: json!({"story": format!("${title_target}.story"), "anchor": 0, "focus": 0}),
            });
            plan.commands
                .push(EngineAction { command: "style.paragraph.apply".into(), params: json!({"name": super::relational::UNIT_HEADING_STYLE}) });
        }
    }
    Ok(())
}

fn mirror_template_action_x(command: &str, params: &mut Map<String, Value>, page_w: f64) {
    if !page_w.is_finite() || page_w <= 0.0 {
        return;
    }
    if matches!(command, "frame.create" | "object.qrCode")
        && let Some(rect) = params.get_mut("rect").and_then(Value::as_array_mut)
        && rect.len() == 4
        && let (Some(x0), Some(x1)) = (rect.first().and_then(Value::as_f64), rect.get(2).and_then(Value::as_f64))
    {
        if let Some(slot0) = rect.get_mut(0) {
            *slot0 = json!(page_w - x1);
        }
        if let Some(slot2) = rect.get_mut(2) {
            *slot2 = json!(page_w - x0);
        }
    }
    if command == "line.create" {
        for key in ["a", "b"] {
            if let Some(pt) = params.get_mut(key).and_then(Value::as_array_mut)
                && let Some(x) = pt.first().and_then(Value::as_f64)
                && let Some(slot0) = pt.get_mut(0)
            {
                *slot0 = json!(page_w - x);
            }
        }
    }
    if command == "path.create"
        && let Some(anchors) = params.get_mut("anchors").and_then(Value::as_array_mut)
    {
        for anchor in anchors {
            if let Some(obj) = anchor.as_object_mut() {
                for key in ["p", "in", "out"] {
                    if let Some(pt) = obj.get_mut(key).and_then(Value::as_array_mut)
                        && let Some(x) = pt.first().and_then(Value::as_f64)
                        && let Some(slot0) = pt.get_mut(0)
                    {
                        *slot0 = json!(page_w - x);
                    }
                }
            }
        }
    }
}

fn compact_slot_snippet(text: Option<&String>, max_chars: usize) -> String {
    let Some(raw) = text else { return String::new() };
    raw.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(max_chars).collect()
}

fn bind_rtl_and_subject_image_prompt(
    base_prompt: &str,
    family_id: &str,
    slot_id: &str,
    slot_purpose: &str,
    page_text: &BTreeMap<String, String>,
    mirror_rtl: bool,
    subject: Option<CurriculumSubject>,
) -> (String, String) {
    let mut prompt = base_prompt.to_owned();
    let mut label = slot_purpose.to_owned();
    let single_card_key = match (family_id, slot_id) {
        ("oxford-vocab-story", "vocab-card-1") => Some("vocab-1"),
        ("oxford-vocab-story", "vocab-card-2") => Some("vocab-2"),
        ("oxford-vocab-story", "vocab-card-3") => Some("vocab-3"),
        ("garnet-computing-systems-lab", "computing-card-1") => Some("term-1"),
        ("garnet-computing-systems-lab", "computing-card-2") => Some("term-2"),
        ("garnet-computing-systems-lab", "computing-card-3") => Some("term-3"),
        ("oxford-phonics-maze", "phonics-card-1") => Some("phonics-1"),
        ("oxford-phonics-maze", "phonics-card-2") => Some("phonics-2"),
        ("oxford-phonics-maze", "phonics-card-3") => Some("phonics-3"),
        ("oxford-song-colours", "colour-card-1") => Some("colour-1"),
        ("oxford-song-colours", "colour-card-2") => Some("colour-2"),
        ("oxford-song-colours", "colour-card-3") => Some("colour-3"),
        _ => None,
    };
    if let Some(text_key) = single_card_key {
        let snippet = compact_slot_snippet(page_text.get(text_key), 64);
        if !snippet.is_empty() {
            prompt.push_str(&format!(
                "\nSINGLE ISOLATED FLASHCARD ILLUSTRATION: Depict '{snippet}' clearly centered on a clean calm background with generous breathing room, matching its linked live caption; draw NO text, letters, or numbers."
            ));
            if !label.contains("[1:1-card]") {
                label.push_str(" [1:1-card]");
            }
        }
    }
    if mirror_rtl {
        if family_id == "oxford-song-colours" && slot_id == "song-scene" {
            for needle in ["Keep the right 34% lighter", "keep the right 34% lighter", "right 34%"] {
                if prompt.contains(needle) {
                    prompt = prompt.replace(needle, "LEFT 34% calm light background space");
                }
            }
            if label.contains("right 34%") {
                label = label.replace("right 34%", "LEFT 34%");
            }
            prompt.push_str(
                "\nCOMPOSITION FOR RTL OVERLAY: Keep the LEFT 34% of the frame calm, uncluttered light background space for the native Arabic card overlay; place the main characters and action in the RIGHT 66%.",
            );
        } else if family_id == "international-curriculum-cover" && slot_id == "cover-hero" {
            if prompt.contains("top-right") {
                prompt = prompt.replace("top-right", "top-left");
            }
            if label.contains("top-right") {
                label = label.replace("top-right", "top-left");
            }
            prompt.push_str("\nCOMPOSITION FOR RTL COVER: Keep the top-left corner calm light background space.");
        }

        let four_card_keys: Option<[&str; 4]> = match (family_id, slot_id) {
            ("oxford-vocab-story", "vocab-strip") => Some(["vocab-1", "vocab-2", "vocab-3", "vocab-4"]),
            ("oxford-phonics-maze", "phonics-cards") => Some(["phonics-1", "phonics-2", "phonics-3", "phonics-4"]),
            ("oxford-song-colours", "paint-pots") => Some(["colour-1", "colour-2", "colour-3", "colour-4"]),
            ("garnet-computing-systems-lab", "computing-terms") => Some(["term-1", "term-2", "term-3", "term-4"]),
            _ => None,
        };
        if let Some([k1, k2, k3, k4]) = four_card_keys {
            let s1 = compact_slot_snippet(page_text.get(k1), 48);
            let s2 = compact_slot_snippet(page_text.get(k2), 48);
            let s3 = compact_slot_snippet(page_text.get(k3), 48);
            let s4 = compact_slot_snippet(page_text.get(k4), 48);
            if !s1.is_empty() && !s4.is_empty() {
                prompt.push_str(&format!(
                    "\nMANDATORY PHYSICAL LEFT-TO-RIGHT 4-PANEL ORDER (for Arabic RTL layout where card 1 sits on the FAR RIGHT and card 4 sits on the FAR LEFT): Divide this wide horizontal strip into 4 equal, cleanly separated side-by-side cards from physical LEFT to RIGHT: [1st / FAR-LEFT card depicts '{s4}']; [2nd / CENTER-LEFT card depicts '{s3}']; [3rd / CENTER-RIGHT card depicts '{s2}']; [4th / FAR-RIGHT card depicts '{s1}']. Keep crisp white vertical gutters between the 4 cards and draw NO text, letters, or numbers."
                ));
                label.push_str(" [rtl-cells]");
            }
        } else if family_id == "oxford-vocab-story" && slot_id == "story-grid" {
            let b1 = compact_slot_snippet(page_text.get("bubble-1"), 64);
            let b2 = compact_slot_snippet(page_text.get("bubble-2"), 64);
            let b3 = compact_slot_snippet(page_text.get("bubble-3"), 64);
            let b4 = compact_slot_snippet(page_text.get("bubble-4"), 64);
            prompt.push_str(&format!(
                "\nMANDATORY 2x2 QUADRANT ORDER (for Arabic RTL reading order): Arrange 4 distinct story panels in a 2x2 grid: [TOP-RIGHT panel (scene 1): '{b1}']; [TOP-LEFT panel (scene 2): '{b2}']; [BOTTOM-RIGHT panel (scene 3): '{b3}']; [BOTTOM-LEFT panel (scene 4): '{b4}']. Draw NO speech bubbles, text, or numbers."
            ));
            label.push_str(" [rtl-cells]");
        } else if family_id == "oxford-fluency-project" && slot_id == "everyday-dialogue" {
            let d1 = compact_slot_snippet(page_text.get("dialogue-1"), 72);
            let d2 = compact_slot_snippet(page_text.get("dialogue-2"), 72);
            prompt.push_str(&format!(
                "\nMANDATORY LEFT-TO-RIGHT 2-PANEL ORDER (for Arabic RTL dialogue): [LEFT half depicts '{d2}']; [RIGHT half depicts '{d1}']. Draw NO speech bubbles, text, or numbers."
            ));
            label.push_str(" [rtl-cells]");
        } else if family_id == "oxford-fluency-project" && slot_id == "craft-project" {
            let st1 = compact_slot_snippet(page_text.get("step-1"), 48);
            let st3 = compact_slot_snippet(page_text.get("step-3"), 48);
            prompt.push_str(&format!(
                "\nMANDATORY LEFT-TO-RIGHT LAYOUT (for Arabic RTL steps): [RIGHT side shows materials and starting step: '{st1}']; [LEFT side shows completed activity and child presenting: '{st3}']. Draw NO text or numbers."
            ));
            label.push_str(" [rtl-cells]");
        } else if family_id == "oxford-skills-time" && slot_id == "listening-grid" {
            prompt.push_str(
                "\nRTL 3-PANEL ORDER: Arrange the 3 distinct scenes from RIGHT (first scene) to CENTER (second scene) to LEFT (third scene) with clean white separation between panels, and draw NO text, letters, numbers, or checkmarks.",
            );
            label.push_str(" [rtl-cells]");
        } else if family_id == "oxford-workbook-print" && slot_id == "workbook-lineart" {
            let sc1 = compact_slot_snippet(page_text.get("scene-1"), 60);
            let sc2 = compact_slot_snippet(page_text.get("scene-2"), 60);
            prompt.push_str(&format!(
                "\nMANDATORY LEFT-TO-RIGHT COLUMN ORDER (for Arabic RTL workbook): [LEFT column line-art matches '{sc2}']; [RIGHT column line-art matches '{sc1}']. Clean black-and-white line art only, NO text or numbers."
            ));
            label.push_str(" [rtl-cells]");
        }
    }
    if subject == Some(CurriculumSubject::IslamicStudies) {
        prompt.push_str(
            "\nISLAMIC EDUCATIONAL VISUAL ETIQUETTE: Whenever a character is eating, drinking, giving, receiving, or reaching for food or objects, they MUST clearly use their RIGHT hand (keep the left hand resting down or away from food). For Du'a (supplication), show both hands raised together at chest level with palms open upward. Depict natural anatomy (two hands per child) and modest, warm attire; never depict prophets or divine beings.",
        );
    }
    if prompt.len() > 8000 {
        let mut end = 8000;
        while end > 0 && !prompt.is_char_boundary(end) {
            end -= 1;
        }
        prompt.truncate(end);
    }
    (prompt, label)
}

#[allow(clippy::too_many_arguments)]
fn append_images(
    plan: &mut ProductionPlan,
    skill_id: &str,
    page: &ContentPage,
    family: &PageFamily,
    mode: OutputMode,
    quantities: Option<NumberTriplet>,
    illustration_style: &[String],
    mirror_page_w: Option<f64>,
    ctx: Option<TemplateCompileContext<'_>>,
) -> Result<(), String> {
    if mode == OutputMode::NativeOnly && !page.illustrations.is_empty() {
        return Err("Native-only template content cannot request images".into());
    }
    let subject = ctx.and_then(|c| c.subject);
    let smart_hybrid = ctx.is_some_and(|c| c.visual_density == designcraft_ai_types::VisualDensityMode::SmartHybrid && !c.explicit_script);
    let total_pages = ctx.map(|c| c.total_pages).unwrap_or(0).max(page.page.saturating_add(1));
    let mirror_rtl = mirror_page_w.is_some();
    let mut slots = BTreeSet::new();
    for image in &page.illustrations {
        let slot = family.image_slots.iter().find(|slot| slot.id == image.slot).ok_or("Unknown illustration slot")?;
        if !slots.insert(&image.slot) || !bounded_text(&image.prompt, 8192) || image.prompt.len() > 8192 || plan.images.len() >= MAX_IMAGES {
            return Err("Illustration prompt, duplicate slot or image count is invalid".into());
        }
        let (prompt, label) = if let Some(values) = quantities.filter(|_| slot.id == "cpa-concrete") {
            // Preserve authored subject/material/style, while refusing freeform
            // count claims that could contradict the validated native quantities.
            if contains_image_quantity_claim(&image.prompt) {
                return Err("CPA concrete illustration must describe subject, material and style without numeric or spelled-out counts; the host supplies the authored quantities. Raster counts still require human visual review".into());
            }
            let prompt = format!(
                "AUTHORITATIVE COUNT: exactly {} objects, arranged as {} in the first group and {} in the second group. Zero means the corresponding group is empty; when total is zero show the requested setting with no counted objects. Preserve the following requested subject, material and style: {}. No additional counted objects, text, letters, numbers, speech bubbles, or page graphics. Approved illustration style: {}",
                values.total,
                values.left,
                values.right,
                image.prompt,
                illustration_style.join("; ")
            );
            (prompt, format!("CPA concrete quantities: {} + {} = {}; raster count pending human review", values.left, values.right, values.total))
        } else if smart_hybrid
            && let Some(seasonal_prompt) =
                super::planner_blueprint::smart_hybrid_seasonal_motif_prompt(skill_id, &family.id, &slot.id, page.page, total_pages)
        {
            (seasonal_prompt, slot.purpose.clone())
        } else {
            bind_rtl_and_subject_image_prompt(&image.prompt, &family.id, &slot.id, &slot.purpose, &page.text, mirror_rtl, subject)
        };
        if prompt.len() > 8192 {
            return Err("Bound CPA illustration prompt exceeds its limit".into());
        }
        let rect = if let Some(page_w) = mirror_page_w.filter(|w| w.is_finite() && *w > 0.0) {
            [page_w - slot.rect[2], slot.rect[1], page_w - slot.rect[0], slot.rect[3]]
        } else {
            slot.rect
        };
        plan.images.push(ImagePlacement {
            family_id: Some(family.id.clone()),
            slot_id: Some(slot.id.clone()),
            repeat_rects: Vec::new(),
            repeat_groups: Vec::new(),
            prompt,
            page: usize::try_from(page.page).map_err(|_| "Invalid image page")?,
            rect,
            label,
        });
    }
    Ok(())
}

fn validate_tree(value: &Value, depth: usize, remaining: &mut usize) -> Result<(), String> {
    if depth > 24 || *remaining == 0 {
        return Err("Native template nesting or node count exceeds its limit".into());
    }
    *remaining = remaining.saturating_sub(1);
    match value {
        Value::String(text) if text.len() > MAX_JSON => return Err("Native template string exceeds its limit".into()),
        Value::Array(values) => {
            for value in values {
                validate_tree(value, depth + 1, remaining)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                validate_tree(value, depth + 1, remaining)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn rebase(value: &mut Value, index: usize, offset: usize) -> Result<(), String> {
    match value {
        Value::String(text) => {
            let (raw_body, was_braced) = if let Some(inner) = text.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
                if inner.contains(['$', '{', '}']) {
                    return Err("Interpolated native template references are unsupported; use $N.path".into());
                }
                (Some(inner), true)
            } else if text.contains("${") {
                return Err("Interpolated native template references are unsupported; use $N.path".into());
            } else {
                (text.strip_prefix('$'), false)
            };
            if let Some(body) = raw_body {
                let looks_like_reference =
                    was_braced || body == "last" || body.starts_with("last.") || body.bytes().next().is_some_and(|b| b.is_ascii_digit());
                if looks_like_reference {
                    let (source_tok, path_opt) = match body.split_once('.') {
                        Some((s, p)) => (s, Some(p)),
                        None => (body, None),
                    };
                    let source = if source_tok == "last" {
                        index.checked_sub(1).ok_or("Native template $last reference has no preceding action in the same page")?
                    } else {
                        source_tok.parse::<usize>().map_err(|_| "Invalid native template reference index")?
                    };
                    if source >= index {
                        return Err("Native template references must address an earlier result in the same page".into());
                    }
                    if let Some(path) = path_opt
                        && (path.is_empty()
                            || !path
                                .split('.')
                                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')))
                    {
                        return Err("Native template references must address an earlier result in the same page".into());
                    }
                    let rebased = source.checked_add(offset).ok_or("Native template reference index overflow")?;
                    *text = match path_opt {
                        Some(path) => format!("${rebased}.{path}"),
                        None => format!("${rebased}"),
                    };
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                rebase(value, index, offset)?;
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                rebase(value, index, offset)?;
            }
        }
        _ => {}
    }
    Ok(())
}
