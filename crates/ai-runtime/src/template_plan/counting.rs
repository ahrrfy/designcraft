//! Semantic CPA counting-frame topology validation and authored quantity binding.
use super::MAX_ACTIONS;
use super::curriculum::{CurriculumSubject, detect_curriculum_subject, normalize_curriculum_query};
use designcraft_ai_skills::SkillSnapshot;
use designcraft_ai_types::EngineAction;
use designcraft_ai_types::design::PageFamily;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn has_unbound_counting_frame(template: &[EngineAction]) -> bool {
    template.iter().any(|action| {
        action.command == "object.label" && action.params.get("label").and_then(Value::as_str).is_some_and(|label| label.starts_with("part.cpa.tf."))
    })
}

pub(super) fn counting_family(family: &PageFamily, template: &[EngineAction]) -> bool {
    family.id == CAMBRIDGE_ID || has_unbound_counting_frame(template)
}

// Dynamic semantic CPA topology resolution: validates all labeled ten-frame,
// number-bond, text, and graphic components by semantic label/slot metadata
// rather than brittle step indices or fixed action counts.
pub(super) const CAMBRIDGE_ID: &str = "cambridge-cpa-grammar";
pub(super) const CAMBRIDGE_EXTRA_ACTIONS: usize = 29; // 2 divider triples, 7 counter triples, group + label.

struct CpaTopology {
    outer_rect: [f64; 4],
    v1_line: usize,
    v2_line: usize,
    tokens: [(usize, usize); 3],
    tenframe_group: usize,
}

fn parse_step_id_ref(token: &str, current_index: usize) -> Option<usize> {
    let body = token.strip_prefix("${").and_then(|s| s.strip_suffix('}')).or_else(|| token.strip_prefix('$'))?;
    let step_str = body.strip_suffix(".id")?;
    let step = if step_str == "last" { current_index.checked_sub(1)? } else { step_str.parse::<usize>().ok()? };
    (step < current_index).then_some(step)
}

fn action_rect4(action: &EngineAction, key: &str) -> Option<[f64; 4]> {
    let items = action.params.get(key)?.as_array()?;
    if items.len() != 4 {
        return None;
    }
    let mut out = [0.0; 4];
    for (dst, val) in out.iter_mut().zip(items) {
        let n = val.as_f64()?;
        if !n.is_finite() {
            return None;
        }
        *dst = n;
    }
    Some(out)
}

fn action_point2(action: &EngineAction, key: &str) -> Option<[f64; 2]> {
    let items = action.params.get(key)?.as_array()?;
    if items.len() != 2 {
        return None;
    }
    let x = items.first()?.as_f64()?;
    let y = items.get(1)?.as_f64()?;
    (x.is_finite() && y.is_finite()).then_some([x, y])
}

fn collect_single_id_labels(template: &[EngineAction]) -> Option<BTreeMap<&str, usize>> {
    const ALLOWED_TF_LABELS: [&str; 7] = [
        "part.cpa.tf.outer",
        "part.cpa.tf.mid-h",
        "part.cpa.tf.v1",
        "part.cpa.tf.v2",
        "part.cpa.tf.token-1",
        "part.cpa.tf.token-2",
        "part.cpa.tf.token-3",
    ];
    let mut labels = BTreeMap::new();
    for (idx, action) in template.iter().enumerate() {
        if action.command != "object.label" {
            continue;
        }
        let label = action.params.get("label").and_then(Value::as_str)?;
        if label.starts_with("part.cpa.tf.") && !ALLOWED_TF_LABELS.contains(&label) {
            return None;
        }
        let Some(ids) = action.params.get("ids").and_then(Value::as_array) else {
            if label.starts_with("part.cpa.") || label.starts_with("group.cpa.") {
                return None;
            }
            continue;
        };
        if ids.len() == 1
            && let Some(id_str) = ids.first().and_then(Value::as_str)
            && let Some(target) = parse_step_id_ref(id_str, idx)
        {
            if labels.insert(label, target).is_some() && (label.starts_with("part.cpa.") || label.starts_with("group.cpa.")) {
                return None;
            }
        } else if label.starts_with("part.cpa.") || label.starts_with("group.cpa.") {
            return None;
        }
    }
    Some(labels)
}

fn resolve_cpa_topology_actions(template: &[EngineAction]) -> Option<CpaTopology> {
    if template.len().checked_add(CAMBRIDGE_EXTRA_ACTIONS).is_none_or(|n| n > MAX_ACTIONS) {
        return None;
    }
    let labels = collect_single_id_labels(template)?;
    let outer_idx = *labels.get("part.cpa.tf.outer")?;
    let outer_act = template.get(outer_idx)?;
    if outer_act.command != "frame.create"
        || outer_act.params.get("shape").and_then(Value::as_str) != Some("rectangle")
        || outer_act.params.get("content").and_then(Value::as_str) != Some("unassigned")
    {
        return None;
    }
    let outer_rect = action_rect4(outer_act, "rect")?;
    let [x0, y0, x1, y1] = outer_rect;
    let width = x1 - x0;
    let height = y1 - y0;
    if width < 50.0 || height < 20.0 {
        return None;
    }

    let mid_h_idx = *labels.get("part.cpa.tf.mid-h")?;
    let mid_h_act = template.get(mid_h_idx)?;
    if mid_h_act.command != "line.create" {
        return None;
    }
    let mid_a = action_point2(mid_h_act, "a")?;
    let mid_b = action_point2(mid_h_act, "b")?;
    let mid_y = (y0 + y1) * 0.5;
    if (mid_a[0] - x0).abs() > 0.05 || (mid_b[0] - x1).abs() > 0.05 || (mid_a[1] - mid_y).abs() > 0.05 || (mid_b[1] - mid_y).abs() > 0.05 {
        return None;
    }

    let v1_line = *labels.get("part.cpa.tf.v1")?;
    let v2_line = *labels.get("part.cpa.tf.v2")?;
    if v1_line == v2_line {
        return None;
    }
    for v_idx in [v1_line, v2_line] {
        let v_act = template.get(v_idx)?;
        if v_act.command != "line.create" {
            return None;
        }
        let va = action_point2(v_act, "a")?;
        let vb = action_point2(v_act, "b")?;
        if (va[0] - vb[0]).abs() > 0.05 || va[0] <= x0 + 1.0 || va[0] >= x1 - 1.0 || (va[1] - y0).abs() > 0.05 || (vb[1] - y1).abs() > 0.05 {
            return None;
        }
    }

    let mut tokens = [(0usize, 0usize); 3];
    for (slot_out, token_label) in tokens.iter_mut().zip(["part.cpa.tf.token-1", "part.cpa.tf.token-2", "part.cpa.tf.token-3"]) {
        let frame_idx = *labels.get(token_label)?;
        let frame_act = template.get(frame_idx)?;
        if frame_act.command != "frame.create"
            || frame_act.params.get("shape").and_then(Value::as_str) != Some("ellipse")
            || frame_act.params.get("content").and_then(Value::as_str) != Some("unassigned")
        {
            return None;
        }
        let tr = action_rect4(frame_act, "rect")?;
        if tr[0] < x0 - 0.05 || tr[1] < y0 - 0.05 || tr[2] > x1 + 0.05 || tr[3] > y1 + 0.05 {
            return None;
        }
        let mut fill_matches = template.iter().enumerate().filter(|(idx, act)| {
            act.command == "object.fill"
                && act.params.get("swatch").and_then(Value::as_str).is_some()
                && act.params.get("ids").and_then(Value::as_array).is_some_and(|ids| {
                    ids.len() == 1 && ids.first().and_then(Value::as_str).and_then(|s| parse_step_id_ref(s, *idx)) == Some(frame_idx)
                })
        });
        let (fill_idx, _) = fill_matches.next()?;
        if fill_matches.next().is_some() {
            return None;
        }
        *slot_out = (frame_idx, fill_idx);
    }
    if tokens[0].0 == tokens[1].0 || tokens[0].0 == tokens[2].0 || tokens[1].0 == tokens[2].0 {
        return None;
    }

    let tenframe_group = *labels.get("group.cpa.pictorial-tenframe")?;
    let group_act = template.get(tenframe_group)?;
    if group_act.command != "object.group" {
        return None;
    }
    let group_members: BTreeSet<usize> =
        group_act.params.get("ids")?.as_array()?.iter().filter_map(|v| v.as_str().and_then(|s| parse_step_id_ref(s, tenframe_group))).collect();
    for required_member in [outer_idx, mid_h_idx, v1_line, v2_line, tokens[0].0, tokens[1].0, tokens[2].0] {
        if !group_members.contains(&required_member) {
            return None;
        }
    }

    for nb_label in ["part.cpa.nb.whole", "part.cpa.nb.part-1", "part.cpa.nb.part-2"] {
        let nb_idx = *labels.get(nb_label)?;
        let nb_act = template.get(nb_idx)?;
        if nb_act.command != "frame.create" || nb_act.params.get("shape").and_then(Value::as_str) != Some("ellipse") {
            return None;
        }
    }
    let nb_group_idx = *labels.get("group.cpa.abstract-numberbond")?;
    if template.get(nb_group_idx)?.command != "object.group" {
        return None;
    }

    Some(CpaTopology { outer_rect, v1_line, v2_line, tokens, tenframe_group })
}

pub(super) fn bindable_counting_family(family: &PageFamily, template: &[EngineAction]) -> bool {
    const REQUIRED_CPA_TEXT_SLOTS: [(&str, &str); 12] = [
        ("title-main", "part.cambridge-cpa-grammar.title-main"),
        ("header-tag", "part.ui.header.tag-text"),
        ("grammar-prompt", "part.cambridge-cpa-grammar.grammar-prompt"),
        ("grammar-q", "part.bubble.grammar-q.text"),
        ("grammar-a", "part.bubble.grammar-a.text"),
        ("grammar-rule-1", "part.grammar.rule-1-txt"),
        ("grammar-rule-2", "part.grammar.rule-2-txt"),
        ("grammar-focus", "part.grammar.contraction-txt"),
        ("cpa-prompt", "part.cambridge-cpa-grammar.cpa-prompt"),
        ("cpa-concrete-label", "part.cpa.concrete.txt"),
        ("cpa-pictorial-label", "part.cpa.pictorial.txt"),
        ("cpa-bond-label", "part.cpa.eq.text"),
    ];
    const REQUIRED_CPA_IMAGE_SLOTS: [(&str, &str); 2] = [("grammar-scene", "part.art.grammar-scene"), ("cpa-concrete", "part.art.cpa-concrete")];
    if family.id != CAMBRIDGE_ID
        || family.text_slots.len() != REQUIRED_CPA_TEXT_SLOTS.len()
        || family.image_slots.len() != REQUIRED_CPA_IMAGE_SLOTS.len()
    {
        return false;
    }
    let Some(labels) = collect_single_id_labels(template) else {
        return false;
    };
    let text_ok = REQUIRED_CPA_TEXT_SLOTS.iter().all(|(slot_id, expected_label)| {
        let Some(slot) = family.text_slots.iter().find(|s| s.id == *slot_id) else {
            return false;
        };
        labels.get(expected_label) == Some(&slot.command)
            && template.get(slot.command).is_some_and(|act| {
                act.command == "frame.create"
                    && act.params.get("content").and_then(Value::as_str) == Some("text")
                    && act.params.get("text").and_then(Value::as_str).is_some()
            })
    });
    let images_ok = REQUIRED_CPA_IMAGE_SLOTS.iter().all(|(slot_id, expected_label)| {
        let Some(slot) = family.image_slots.iter().find(|s| s.id == *slot_id) else {
            return false;
        };
        if !slot.views.is_empty() || slot.crop.is_some() || slot.source_aspect_ratio.is_some() {
            return false;
        }
        let Some(&frame_idx) = labels.get(expected_label) else {
            return false;
        };
        template
            .get(frame_idx)
            .and_then(|act| {
                (act.command == "frame.create" && act.params.get("content").and_then(Value::as_str) == Some("graphic"))
                    .then(|| action_rect4(act, "rect"))
                    .flatten()
            })
            .is_some_and(|rect| rect.iter().zip(slot.rect).all(|(a, b)| (a - b).abs() <= 0.05))
    });
    text_ok && images_ok && resolve_cpa_topology_actions(template).is_some()
}

/// Math can retain the template path only when every fixed counting template is
/// the supported topology with explicit CPA slot bindings. No quantity defaults.
pub(crate) fn requires_adaptive_math(brief: &str, skills: &[SkillSnapshot]) -> bool {
    let lower = brief.to_lowercase();
    let math_request = detect_curriculum_subject(brief) == CurriculumSubject::Mathematics
        || ["math", "count", "five-frame", "ten-frame", "رياضيات", "العد", "أعداد", "أرقام", "إطار عشري", "إطار خماسي"]
            .iter()
            .any(|word| lower.contains(word));
    math_request
        && skills.iter().any(|skill| {
            skill.templates.iter().any(|(path, template)| {
                (has_unbound_counting_frame(template)
                    || skill
                        .design_profile
                        .as_ref()
                        .is_some_and(|profile| profile.page_families.iter().any(|family| family.template == *path && family.id == CAMBRIDGE_ID)))
                    && !skill.design_profile.as_ref().is_some_and(|profile| {
                        profile.page_families.iter().any(|family| family.template == *path && bindable_counting_family(family, template))
                    })
            })
        })
}

/// Determines when production should transition from fixed textbook template slots
/// to Adaptive Mastermind Native Tool Generation (`frame.create`, `path.create`,
/// `table.insert`, `object.fill`, `object.cornerOptions`, `object.dropShadow`, etc.):
/// 1. Math requests with unbound counting templates (`requires_adaptive_math`),
/// 2. Non-textbook publication formats (`Brochure`, `Magazine`, `Report`, `PosterOrCertificate`),
/// 3. Fixed plant-anatomy templates (`explore-plant-parts` with `leaves-ar`) when the brief is not Science.
pub(crate) fn requires_adaptive_mastermind(brief: &str, skills: &[SkillSnapshot]) -> bool {
    if requires_adaptive_math(brief, skills) {
        return true;
    }
    let subject = detect_curriculum_subject(brief);
    if matches!(
        subject,
        CurriculumSubject::Brochure | CurriculumSubject::Magazine | CurriculumSubject::Report | CurriculumSubject::PosterOrCertificate
    ) {
        return true;
    }
    let has_plant_bound_slots = skills.iter().any(|skill| {
        skill.design_profile.as_ref().is_some_and(|profile| {
            profile
                .page_families
                .iter()
                .any(|family| family.id == "explore-plant-parts" && family.text_slots.iter().any(|slot| slot.id == "leaves-ar"))
        })
    });
    has_plant_bound_slots && subject != CurriculumSubject::Science
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct NumberTriplet {
    pub(super) left: u32,
    pub(super) right: u32,
    pub(super) total: u32,
}

fn decimal_digit(c: char) -> Option<u32> {
    match c {
        '0'..='9' => c.to_digit(10),
        '\u{0660}'..='\u{0669}' => Some(u32::from(c) - u32::from('\u{0660}')),
        '\u{06f0}'..='\u{06f9}' => Some(u32::from(c) - u32::from('\u{06f0}')),
        _ => None,
    }
}

fn equation_spacing(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn equation_operator(c: char) -> bool {
    matches!(
        c,
        '+' | '='
            | '-'
            | '\u{2212}'
            | '*'
            | '\u{00d7}'
            | '/'
            | '\u{00f7}'
            | '<'
            | '>'
            | '\u{2264}'
            | '\u{2265}'
            | '－'
            | '﹣'
            | '‐'
            | '‑'
            | '‒'
            | '–'
            | '—'
            | '±'
            | '∓'
            | '＋'
            | '﹢'
    )
}

fn equation_number(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Result<u32, String> {
    while chars.peek().is_some_and(|c| equation_spacing(*c)) {
        let _ = chars.next();
    }
    let mut number = None::<u32>;
    while let Some(digit) = chars.peek().and_then(|c| decimal_digit(*c)) {
        let _ = chars.next();
        number = Some(number.unwrap_or(0).checked_mul(10).and_then(|n| n.checked_add(digit)).ok_or("CPA quantity overflow")?);
    }
    number.ok_or_else(|| "CPA equation requires nonnegative integers".into())
}

fn equation_symbol(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, symbol: char) -> Result<(), String> {
    while chars.peek().is_some_and(|c| equation_spacing(*c)) {
        let _ = chars.next();
    }
    if chars.next() != Some(symbol) {
        return Err("CPA requires one explicit left + right = total addition equation".into());
    }
    Ok(())
}

fn additive_triplet(text: &str) -> Result<NumberTriplet, String> {
    let start = text
        .char_indices()
        .find(|(_, c)| decimal_digit(*c).is_some())
        .map(|(index, _)| index)
        .ok_or("CPA requires an authored addition equation; template counter counts are never defaults")?;
    let prefix = text.get(..start).ok_or("Invalid CPA equation boundary")?;
    if prefix.chars().any(|c| c.is_numeric() || equation_operator(c)) {
        return Err("CPA equation prefix contains unsupported quantities or operators".into());
    }
    let mut chars = text.get(start..).ok_or("Invalid CPA equation boundary")?.chars().peekable();
    let left = equation_number(&mut chars)?;
    equation_symbol(&mut chars, '+')?;
    let right = equation_number(&mut chars)?;
    equation_symbol(&mut chars, '=')?;
    let total = equation_number(&mut chars)?;
    if chars.any(|c| c.is_numeric() || equation_operator(c)) {
        return Err("CPA requires exactly one addition equation per authored CPA slot".into());
    }
    if left.checked_add(right) != Some(total) || total > 10 {
        return Err("CPA authored quantities must satisfy left + right = total, with total at most 10".into());
    }
    Ok(NumberTriplet { left, right, total })
}

pub(super) fn number_triplet(slots: &BTreeMap<String, String>) -> Result<NumberTriplet, String> {
    let pictorial = additive_triplet(slots.get("cpa-pictorial-label").ok_or("Missing authored pictorial equation")?)?;
    let abstract_value = additive_triplet(slots.get("cpa-bond-label").ok_or("Missing authored abstract equation")?)?;
    if pictorial != abstract_value {
        return Err("Authored pictorial and abstract CPA quantities disagree".into());
    }
    Ok(pictorial)
}

pub(super) fn native_action(command: &str, params: Value) -> EngineAction {
    EngineAction { command: command.into(), params }
}

fn replace_template_param(template: &mut [EngineAction], index: usize, key: &str, value: Value) -> Result<(), String> {
    let params = template.get_mut(index).and_then(|action| action.params.as_object_mut()).ok_or("Supported CPA topology disappeared")?;
    params.insert(key.into(), value);
    Ok(())
}

fn counter_rect_in(outer_rect: [f64; 4], index: u32) -> [f64; 4] {
    let [x0, y0, x1, y1] = outer_rect;
    let col_w = (x1 - x0) / 5.0;
    let row_h = (y1 - y0) / 2.0;
    let x = x0 + (f64::from(index % 5) + 0.5) * col_w;
    let y = y0 + (f64::from(index / 5) + 0.5) * row_h;
    [x - 9.0, y - 9.0, x + 9.0, y + 9.0]
}

fn counter_swatch(index: u32, values: NumberTriplet) -> &'static str {
    if index < values.left {
        "C=15 M=100 Y=100 K=0"
    } else if index < values.total {
        "C=75 M=5 Y=100 K=0"
    } else {
        "[Paper]"
    }
}

pub(super) fn bound_counting_template(template: &[EngineAction], values: NumberTriplet) -> Result<Vec<EngineAction>, String> {
    let topo = resolve_cpa_topology_actions(template).ok_or("Supported CPA topology disappeared")?;
    let [x0, y0, x1, y1] = topo.outer_rect;
    let col_w = (x1 - x0) / 5.0;
    let mut actions = template.to_vec();
    for (index, column) in [(topo.v1_line, 1u32), (topo.v2_line, 2u32)] {
        let x = x0 + f64::from(column) * col_w;
        replace_template_param(&mut actions, index, "a", json!([x, y0]))?;
        replace_template_param(&mut actions, index, "b", json!([x, y1]))?;
    }
    for (counter, (frame, fill)) in [0u32, 1, 2].into_iter().zip(topo.tokens) {
        replace_template_param(&mut actions, frame, "rect", json!(counter_rect_in(topo.outer_rect, counter)))?;
        replace_template_param(&mut actions, fill, "swatch", json!(counter_swatch(counter, values)))?;
    }
    // Append after all original commands: no authored index, text-slot target,
    // original group member or result reference changes its meaning.
    let mut members = vec![json!(format!("${}.id", topo.tenframe_group))];
    for column in [3u32, 4u32] {
        let index = actions.len();
        let x = x0 + f64::from(column) * col_w;
        members.push(json!(format!("${index}.id")));
        actions.push(native_action("line.create", json!({"spread":0,"a":[x,y0],"b":[x,y1]})));
        actions.push(native_action(
            "object.stroke",
            json!({"ids":[format!("${index}.id")],"swatch":"C=100 M=90 Y=10 K=0","tint":1.0,"weight":0.7,"cap":"round","join":"round"}),
        ));
        actions.push(native_action("object.label", json!({"ids":[format!("${index}.id")],"label":format!("part.cpa.tf.v{column}")})));
    }
    for counter in 3u32..10 {
        let index = actions.len();
        members.push(json!(format!("${index}.id")));
        actions.push(native_action(
            "frame.create",
            json!({"spread":0,"rect":counter_rect_in(topo.outer_rect, counter),"content":"unassigned","shape":"ellipse"}),
        ));
        actions.push(native_action("object.fill", json!({"ids":[format!("${index}.id")],"swatch":counter_swatch(counter, values),"tint":1.0})));
        actions.push(native_action("object.label", json!({"ids":[format!("${index}.id")],"label":format!("part.cpa.tf.token-{}",counter + 1)})));
    }
    let group = actions.len();
    actions.push(native_action("object.group", json!({"ids":members})));
    actions.push(native_action("object.label", json!({"ids":[format!("${group}.id")],"label":"group.cpa.pictorial-tenframe.bound"})));
    if template.len().checked_add(CAMBRIDGE_EXTRA_ACTIONS) != Some(actions.len()) {
        return Err("Bound CPA command capacity changed unexpectedly".into());
    }
    Ok(actions)
}

pub(super) fn contains_image_quantity_claim(prompt: &str) -> bool {
    let normalized = normalize_curriculum_query(prompt);
    normalized.chars().any(|c| c.is_numeric())
        || normalized.split(|c: char| !c.is_alphabetic()).any(|word| {
            matches!(
                word,
                "zero"
                    | "one"
                    | "two"
                    | "three"
                    | "four"
                    | "five"
                    | "six"
                    | "seven"
                    | "eight"
                    | "nine"
                    | "ten"
                    | "eleven"
                    | "twelve"
                    | "thirteen"
                    | "fourteen"
                    | "fifteen"
                    | "sixteen"
                    | "seventeen"
                    | "eighteen"
                    | "nineteen"
                    | "twenty"
                    | "hundred"
                    | "thousand"
                    | "pair"
                    | "pairs"
                    | "dozen"
                    | "صفر"
                    | "واحد"
                    | "واحدة"
                    | "واحدا"
                    | "اثنان"
                    | "اثنين"
                    | "اثنتان"
                    | "اثنتين"
                    | "ثلاث"
                    | "ثلاثة"
                    | "اربع"
                    | "أربع"
                    | "اربعة"
                    | "أربعة"
                    | "خمس"
                    | "خمسة"
                    | "ست"
                    | "ستة"
                    | "سبع"
                    | "سبعة"
                    | "ثمان"
                    | "ثماني"
                    | "ثمانية"
                    | "تسع"
                    | "تسعة"
                    | "عشر"
                    | "عشرة"
                    | "عشرين"
                    | "مئة"
                    | "مائة"
                    | "الف"
                    | "ألف"
                    | "زوج"
                    | "زوجان"
                    | "زوجين"
            )
        })
}
