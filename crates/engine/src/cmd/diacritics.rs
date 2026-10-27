//! Arabic diacritic (tashkeel) normalization, script-aware Arabic font replacement helpers, and
//! bounded overset text fitting — general-purpose typographic primitives operating on stories and
//! page ranges without template-coupled labels.

use std::collections::BTreeSet;
use std::sync::Arc;

use designcraft_doc::{Document, Item, Story, StoryId};
use designcraft_fonts::{ArabicFontMatch, ScopedFonts};
use designcraft_geom::Affine;
use serde_json::{Value, json};

use super::{CommandSpec, bad, cmd, has_doc, str_param};
use crate::{Result, Session};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "type.diacritics",
            "Arabic Diacritics…",
            ["Type"],
            None,
            "{mode: none|strip|light|keep, scope?: document|story, pages?: [start, end], story?: id, stories?: [id], excludeLabels?: [text]} → {removed, stories}",
            has_doc,
            run_diacritics
        ),
        cmd!(
            "story.fitOverset",
            "Fit Overset Text",
            [],
            None,
            "{scope?: document|story, pages?: [start, end], story?: id, stories?: [id], minSize?: number} → {fitted, stillOverset}",
            has_doc,
            run_fit_overset
        ),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TashkeelPolicy {
    Keep,
    None,
    Light,
}

impl TashkeelPolicy {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "keep" | "full" | "unchanged" => Some(Self::Keep),
            "none" | "strip" | "plain" | "remove" => Some(Self::None),
            "light" | "minimal" | "essential" => Some(Self::Light),
            _ => None,
        }
    }

    pub fn should_remove(self, ch: char) -> bool {
        match self {
            Self::Keep => false,
            // Removes routine harakat + tanween + shadda + sukun + superscript alef, while keeping
            // hamza/madda above/below (U+0653..=U+0655), base letters, digits, and ligatures (ﷺ).
            Self::None => matches!(ch, '\u{064B}'..='\u{0652}' | '\u{0670}'),
            // Light educational vocalization: strips fatha, damma, kasra, sukun, and superscript
            // alef while preserving shadda (U+0651) and tanween (U+064B..=U+064D).
            Self::Light => matches!(ch, '\u{064E}' | '\u{064F}' | '\u{0650}' | '\u{0652}' | '\u{0670}'),
        }
    }
}

pub(super) fn parse_pages_range(cmd_id: &str, p: &Value, total_pages: usize) -> Result<Option<(usize, usize)>> {
    let Some(val) = p.get("pages") else {
        return Ok(None);
    };
    let arr = val.as_array().ok_or_else(|| bad(cmd_id, "`pages` must be `[start, end]` (1-based inclusive)"))?;
    if arr.len() != 2 {
        return Err(bad(cmd_id, "`pages` must have exactly two 1-based page numbers `[start, end]`"));
    }
    let start = arr.first().and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
    let end = arr.get(1).and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
    if start == 0 || end == 0 || start > end {
        return Err(bad(cmd_id, "`pages` must satisfy `1 <= start <= end`"));
    }
    if total_pages == 0 || start > total_pages {
        return Ok(Some((start, end)));
    }
    let clamped_end = end.min(total_pages);
    Ok(Some((start, clamped_end)))
}

#[derive(Clone, Debug)]
struct StoryPlacement {
    page_num: usize,
    label: String,
    story: StoryId,
}

fn collect_spread_stories(
    items: &[Arc<Item>],
    parent_xf: Affine,
    pages_in_spread: &[(usize, f64, f64)],
    out: &mut Vec<StoryPlacement>,
    depth: usize,
) {
    if depth > 64 {
        return;
    }
    for item in items {
        let world_xf = parent_xf * item.xf;
        let children = item.children();
        if !children.is_empty() {
            collect_spread_stories(children, world_xf, pages_in_spread, out, depth + 1);
            continue;
        }
        let Some(tf) = item.text_frame() else {
            continue;
        };
        let world_bounds = world_xf.transform_rect_bbox(item.inner_bounds());
        let cx = world_bounds.center().x;
        let chosen = pages_in_spread
            .iter()
            .copied()
            .find(|(_, px, pw)| cx >= *px - 0.5 && cx <= *px + *pw + 0.5)
            .or_else(|| pages_in_spread.iter().copied().min_by(|a, b| (cx - (a.1 + a.2 * 0.5)).abs().total_cmp(&(cx - (b.1 + b.2 * 0.5)).abs())));
        let page_num = chosen.map_or(0, |(p, _, _)| p);
        out.push(StoryPlacement { page_num, label: item.label.clone(), story: tf.story });
    }
}

fn collect_document_stories(doc: &Document) -> (usize, Vec<StoryPlacement>) {
    let mut total_pages = 0usize;
    let mut placements = Vec::new();
    for parent in &doc.parents {
        let pages_in_parent: Vec<(usize, f64, f64)> = parent.pages.iter().map(|p| (0usize, p.x, p.width)).collect();
        collect_spread_stories(&parent.items, Affine::IDENTITY, &pages_in_parent, &mut placements, 0);
    }
    for spread in &doc.spreads {
        let mut pages_in_spread = Vec::with_capacity(spread.pages.len());
        for page in &spread.pages {
            total_pages = total_pages.saturating_add(1);
            pages_in_spread.push((total_pages, page.x, page.width));
        }
        collect_spread_stories(&spread.items, Affine::IDENTITY, &pages_in_spread, &mut placements, 0);
    }
    (total_pages, placements)
}

pub(super) fn stories_on_pages(doc: &Document, page_range: Option<(usize, usize)>, exclude_labels: &[String]) -> BTreeSet<StoryId> {
    if page_range.is_none() && exclude_labels.is_empty() {
        return doc.stories.keys().copied().collect();
    }
    let (_, placements) = collect_document_stories(doc);
    let mut stories = BTreeSet::new();
    let mut excluded_stories = BTreeSet::new();
    for meta in &placements {
        let in_range = page_range.is_none_or(|(lo, hi)| meta.page_num > 0 && (lo..=hi).contains(&meta.page_num));
        if !in_range {
            continue;
        }
        if !exclude_labels.is_empty()
            && exclude_labels.iter().any(|pat| !pat.is_empty() && meta.label.to_ascii_lowercase().contains(&pat.to_ascii_lowercase()))
        {
            excluded_stories.insert(meta.story);
        } else {
            stories.insert(meta.story);
        }
    }
    for sid in excluded_stories {
        stories.remove(&sid);
    }
    stories
}

pub fn strip_story_diacritics(st: &mut Story, policy: TashkeelPolicy) -> usize {
    if policy == TashkeelPolicy::Keep {
        return 0;
    }
    let mut total_removed = 0usize;
    let mut pending = vec![st];
    while let Some(cur) = pending.pop() {
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for (idx, ch) in cur.text.char_indices() {
            if policy.should_remove(ch) {
                let end = idx.saturating_add(ch.len_utf8());
                if let Some(last) = ranges.last_mut()
                    && last.1 == idx
                {
                    last.1 = end;
                } else {
                    ranges.push((idx, end));
                }
                total_removed = total_removed.saturating_add(1);
            }
        }
        for (start, end) in ranges.into_iter().rev() {
            cur.delete(start..end);
        }
        pending.extend(cur.tables.values_mut().flat_map(|t| Arc::make_mut(t).cells.iter_mut().map(|c| &mut c.text)));
        pending.extend(cur.notes.iter_mut().chain(&mut cur.endnotes).map(|n| &mut Arc::make_mut(n).text));
    }
    total_removed
}

pub(super) fn is_arabic_letter(ch: char) -> bool {
    matches!(
        ch as u32,
        0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF
    )
}

pub(super) fn is_latin_letter(ch: char) -> bool {
    ch.is_ascii_alphabetic() || matches!(ch as u32, 0x00C0..=0x024F | 0x1E00..=0x1EFF)
}

pub(super) fn style_looks_bold(style: &str, weight: f32) -> bool {
    if weight >= 600.0 {
        return true;
    }
    let lower = style.to_ascii_lowercase();
    ["bold", "black", "heavy", "semibold", "demibold", "demi", "extrabold", "عريض"].iter().any(|kw| lower.contains(kw))
}

pub(super) fn replace_arabic_in_story(
    st: &mut Story,
    original_styles: &designcraft_doc::Styles,
    db: &ScopedFonts<'_>,
    matched: &ArabicFontMatch,
    source_family_filter: Option<&str>,
    source_style_filter: Option<&str>,
    redefined_para_styles: &BTreeSet<String>,
    redefined_char_styles: &BTreeSet<String>,
) -> usize {
    let mut changed = 0usize;
    let reg_face = db.face(&matched.regular_family, &matched.regular_style);
    let bold_face = db.face(&matched.bold_family, &matched.bold_style);

    let mut pending = vec![st];
    while let Some(cur) = pending.pop() {
        let mut updates: Vec<(std::ops::Range<usize>, String, String)> = Vec::new();
        for (pi, paragraph) in cur.para_ranges().iter().enumerate() {
            let Some(para_fmt) = cur.paras.get(pi) else { continue };
            let (_, base) = original_styles.resolve_para(para_fmt);
            let para_style_redefined = redefined_para_styles.contains(&para_fmt.style);
            for (run, char_fmt) in cur.runs() {
                let start = run.start.max(paragraph.start);
                let end = run.end.min(paragraph.end);
                if start >= end {
                    continue;
                }
                let Some(slice) = cur.text.get(start..end) else { continue };
                let has_arabic = slice.chars().any(is_arabic_letter);
                let has_latin = slice.chars().any(is_latin_letter);
                let inherited_style_redefined =
                    char_fmt.over.font_family.is_none() && (para_style_redefined || redefined_char_styles.contains(&char_fmt.style));
                if !(has_arabic || has_latin && inherited_style_redefined) {
                    continue;
                }
                let effective = original_styles.resolve_char(&base, char_fmt);

                if !has_arabic {
                    // Pure-Latin run whose inherited style is being redefined to an Arabic font:
                    // pin its original Latin font so English words do not switch to the Arabic face.
                    for (rel_idx, ch) in slice.char_indices() {
                        if !is_latin_letter(ch) {
                            continue;
                        }
                        let abs_start = start.saturating_add(rel_idx);
                        let abs_end = start.saturating_add(rel_idx.saturating_add(ch.len_utf8()));
                        if let Some(last) = updates.last_mut()
                            && last.0.end == abs_start
                            && last.1 == effective.font_family
                            && last.2 == effective.font_style
                        {
                            last.0.end = abs_end;
                        } else {
                            updates.push((abs_start..abs_end, effective.font_family.clone(), effective.font_style.clone()));
                        }
                    }
                    continue;
                }

                if let Some(fam_filter) = source_family_filter
                    && fam_filter != "*"
                    && !effective.font_family.eq_ignore_ascii_case(fam_filter)
                {
                    continue;
                }
                if let Some(st_filter) = source_style_filter
                    && !effective.font_style.eq_ignore_ascii_case(st_filter)
                {
                    continue;
                }
                let (eff_weight, _) = db.traits_of(&effective.font_family, &effective.font_style);
                let is_bold = style_looks_bold(&effective.font_style, eff_weight);
                let (primary_fam, primary_style, primary_face) = if is_bold {
                    (&matched.bold_family, &matched.bold_style, &bold_face)
                } else {
                    (&matched.regular_family, &matched.regular_style, &reg_face)
                };
                let current_face = db.face(&effective.font_family, &effective.font_style);

                // Split out Latin sub-runs (preserving their Latin font even if the paragraph style
                // is redefined) AND preserve covering fallback fonts for special symbols (`ﷺ`, `✓`).
                for (rel_idx, ch) in slice.char_indices() {
                    let abs_start = start.saturating_add(rel_idx);
                    let abs_end = start.saturating_add(rel_idx.saturating_add(ch.len_utf8()));
                    if is_latin_letter(ch) {
                        if inherited_style_redefined {
                            if let Some(last) = updates.last_mut()
                                && last.0.end == abs_start
                                && last.1 == effective.font_family
                                && last.2 == effective.font_style
                            {
                                last.0.end = abs_end;
                            } else {
                                updates.push((abs_start..abs_end, effective.font_family.clone(), effective.font_style.clone()));
                            }
                        }
                        continue;
                    }
                    let (chosen_fam, chosen_style) = if matches!(ch, '\u{E000}' | '\u{E007}' | '\u{E008}') {
                        if primary_face.covers('1') && primary_face.covers('A') {
                            (primary_fam.clone(), primary_style.clone())
                        } else if let Some((fb_fam, fb_style)) = db.arabic_glyph_fallback('A', is_bold) {
                            (fb_fam, fb_style)
                        } else if !current_face.is_placeholder() && current_face.covers('1') && current_face.covers('A') {
                            (effective.font_family.clone(), effective.font_style.clone())
                        } else {
                            (primary_fam.clone(), primary_style.clone())
                        }
                    } else if ch.is_whitespace() || ch.is_control() || ('\u{E000}'..='\u{E1FF}').contains(&ch) || primary_face.covers(ch) {
                        (primary_fam.clone(), primary_style.clone())
                    } else if !current_face.is_placeholder() && current_face.covers(ch) {
                        (effective.font_family.clone(), effective.font_style.clone())
                    } else if let Some((fb_fam, fb_style)) = db.arabic_glyph_fallback(ch, is_bold) {
                        (fb_fam, fb_style)
                    } else {
                        (primary_fam.clone(), primary_style.clone())
                    };
                    if !inherited_style_redefined
                        && effective.font_family.eq_ignore_ascii_case(&chosen_fam)
                        && effective.font_style.eq_ignore_ascii_case(&chosen_style)
                    {
                        continue;
                    }
                    if let Some(last) = updates.last_mut()
                        && last.0.end == abs_start
                        && last.1 == chosen_fam
                        && last.2 == chosen_style
                    {
                        last.0.end = abs_end;
                    } else {
                        updates.push((abs_start..abs_end, chosen_fam, chosen_style));
                    }
                }
            }
        }
        for (range, fam, style) in updates {
            cur.format_chars(range, |fmt| {
                fmt.over.font_family = Some(fam.clone());
                fmt.over.font_style = Some(style.clone());
            });
            changed = changed.saturating_add(1);
        }
        pending.extend(cur.tables.values_mut().flat_map(|t| Arc::make_mut(t).cells.iter_mut().map(|c| &mut c.text)));
        pending.extend(cur.notes.iter_mut().chain(&mut cur.endnotes).map(|n| &mut Arc::make_mut(n).text));
    }
    changed
}

fn scale_story_tree(base: &Story, original_styles: &designcraft_doc::Styles, scale: f64, min_size: f64) -> (Story, bool) {
    let mut scaled = base.clone();
    let mut shrunk_any = false;
    let mut pending: Vec<&mut Story> = vec![&mut scaled];
    while let Some(cur) = pending.pop() {
        let para_ranges = cur.para_ranges();
        let mut updates: Vec<(std::ops::Range<usize>, f64, Option<designcraft_doc::Leading>)> = Vec::new();
        for (pi, pr) in para_ranges.iter().enumerate() {
            let Some(pfmt) = cur.paras.get(pi) else { continue };
            let (_, base_char) = original_styles.resolve_para(pfmt);
            for (rr, cfmt) in cur.runs() {
                let s0 = rr.start.max(pr.start);
                let s1 = rr.end.min(pr.end);
                if s0 >= s1 {
                    continue;
                }
                let resolved = original_styles.resolve_char(&base_char, cfmt);
                let orig_size = cfmt.over.size.unwrap_or(resolved.size);
                if orig_size > min_size + 1e-6 {
                    let raw_next = (orig_size * scale).max(min_size);
                    let next_sz = ((raw_next * 100.0).round() / 100.0).max(min_size);
                    if orig_size - next_sz > 1e-6 {
                        let next_lead = match resolved.leading {
                            designcraft_doc::Leading::Points(pts) if orig_size > 1e-6 => {
                                let scaled_pts = ((pts * (next_sz / orig_size)).max(next_sz) * 100.0).round() / 100.0;
                                Some(designcraft_doc::Leading::Points(scaled_pts))
                            }
                            _ => None,
                        };
                        updates.push((s0..s1, next_sz, next_lead));
                    }
                }
            }
        }
        for (range, next_sz, next_lead) in updates {
            cur.format_chars(range, |fmt| {
                fmt.over.size = Some(next_sz);
                if let Some(lead) = next_lead {
                    fmt.over.leading = Some(lead);
                }
            });
            shrunk_any = true;
        }
        pending.extend(cur.tables.values_mut().flat_map(|t| Arc::make_mut(t).cells.iter_mut().map(|c| &mut c.text)));
        pending.extend(cur.notes.iter_mut().chain(&mut cur.endnotes).map(|n| &mut Arc::make_mut(n).text));
    }
    (scaled, shrunk_any)
}

fn fit_overset_stories(doc: &mut Document, target_stories: &BTreeSet<StoryId>, min_size: f64) -> (usize, usize) {
    let cache = designcraft_compose::Cache::new();
    let mut fitted = 0usize;
    let mut still_overset = 0usize;
    let original_styles = doc.styles.clone();
    for &sid in target_stories {
        let Some(st_ref) = doc.stories.get(&sid) else { continue };
        if st_ref.frames.is_empty() || st_ref.text.is_empty() {
            continue;
        }
        if !cache.get(doc, sid, None).is_overset() {
            continue;
        }
        let base_story = st_ref.as_ref().clone();
        // Check floor (`scale = 0.0` clamps every run > min_size down to min_size).
        let (min_story, shrunk_at_min) = scale_story_tree(&base_story, &original_styles, 0.0, min_size);
        if !shrunk_at_min {
            still_overset = still_overset.saturating_add(1);
            continue;
        }
        if let Some(slot) = doc.story_mut(sid) {
            *slot = min_story;
        }
        if cache.get(doc, sid, None).is_overset() {
            still_overset = still_overset.saturating_add(1);
            continue;
        }

        // Binary-search the largest proportional scale factor in [0.0, 1.0] that fits with zero overset.
        let mut lo = 0.0_f64;
        let mut hi = 1.0_f64;
        for _ in 0..14 {
            let mid = 0.5 * (lo + hi);
            let (candidate, _) = scale_story_tree(&base_story, &original_styles, mid, min_size);
            if let Some(slot) = doc.story_mut(sid) {
                *slot = candidate;
            }
            if !cache.get(doc, sid, None).is_overset() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let (best_story, _) = scale_story_tree(&base_story, &original_styles, lo, min_size);
        if let Some(slot) = doc.story_mut(sid) {
            *slot = best_story;
        }
        if cache.get(doc, sid, None).is_overset() {
            let (floor_story, _) = scale_story_tree(&base_story, &original_styles, 0.0, min_size);
            if let Some(slot) = doc.story_mut(sid) {
                *slot = floor_story;
            }
        }
        fitted = fitted.saturating_add(1);
    }
    (fitted, still_overset)
}

fn resolve_explicit_stories(cmd_id: &str, p: &Value, doc_state: &crate::DocState) -> Result<Option<BTreeSet<StoryId>>> {
    let doc = &doc_state.doc;
    let mut explicit = BTreeSet::new();
    if let Some(val) = p.get("story") {
        let raw = val.as_u64().ok_or_else(|| bad(cmd_id, "`story` must be a story id integer"))?;
        let sid = StoryId(raw);
        if !doc.stories.contains_key(&sid) {
            return Err(bad(cmd_id, format!("no such story {}", sid.0)));
        }
        explicit.insert(sid);
    }
    if let Some(val) = p.get("stories") {
        let arr = val.as_array().ok_or_else(|| bad(cmd_id, "`stories` must be an array of story ids"))?;
        for entry in arr {
            let raw = entry.as_u64().ok_or_else(|| bad(cmd_id, "`stories` entries must be integer story ids"))?;
            let sid = StoryId(raw);
            if !doc.stories.contains_key(&sid) {
                return Err(bad(cmd_id, format!("no such story {}", sid.0)));
            }
            explicit.insert(sid);
        }
        if explicit.is_empty() {
            return Ok(Some(explicit));
        }
    }
    if !explicit.is_empty() {
        return Ok(Some(explicit));
    }
    if let Some(scope) = str_param(p, "scope") {
        match scope.trim().to_ascii_lowercase().as_str() {
            "document" => {}
            "story" => {
                if let Some(ts) = doc_state.selection.text {
                    if !doc.stories.contains_key(&ts.story) {
                        return Err(bad(cmd_id, format!("no such story {}", ts.story.0)));
                    }
                    explicit.insert(ts.story);
                } else {
                    for &item_id in &doc_state.selection.items {
                        if let Some(tf) = doc.item(item_id).and_then(|it| it.text_frame()) {
                            explicit.insert(tf.story);
                        }
                    }
                }
                if explicit.is_empty() {
                    return Err(bad(cmd_id, "`scope: \"story\"` requires `story`, `stories`, or an active text/frame selection"));
                }
                return Ok(Some(explicit));
            }
            _ => return Err(bad(cmd_id, "`scope` must be `document` or `story`")),
        }
    }
    Ok(None)
}

fn run_diacritics(s: &mut Session, p: &Value) -> Result<Value> {
    let mode_raw = str_param(p, "mode").ok_or_else(|| bad("type.diacritics", "`mode` required (`none`, `light`, or `keep`)"))?;
    let policy = TashkeelPolicy::parse(mode_raw).ok_or_else(|| bad("type.diacritics", "`mode` must be `none`, `strip`, `light`, or `keep`"))?;
    let doc_state = s.doc()?;
    let total_pages = doc_state.doc.page_count();
    let page_range = parse_pages_range("type.diacritics", p, total_pages)?;
    let explicit_stories = resolve_explicit_stories("type.diacritics", p, doc_state)?;
    let exclude_labels: Vec<String> = p
        .get("excludeLabels")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();

    if policy == TashkeelPolicy::Keep {
        return Ok(json!({"removed": 0, "stories": 0}));
    }

    s.edit(|doc, _| {
        let targets: BTreeSet<StoryId> = match explicit_stories {
            Some(sids) => sids,
            None => stories_on_pages(doc, page_range, &exclude_labels),
        };
        let mut removed = 0usize;
        let mut touched = 0usize;
        for sid in targets {
            if let Some(st) = doc.story_mut(sid) {
                let n = strip_story_diacritics(st, policy);
                if n > 0 {
                    removed = removed.saturating_add(n);
                    touched = touched.saturating_add(1);
                }
            }
        }
        Ok(json!({"removed": removed, "stories": touched}))
    })
}

fn run_fit_overset(s: &mut Session, p: &Value) -> Result<Value> {
    let doc_state = s.doc()?;
    let total_pages = doc_state.doc.page_count();
    let page_range = parse_pages_range("story.fitOverset", p, total_pages)?;
    let explicit_stories = resolve_explicit_stories("story.fitOverset", p, doc_state)?;
    let min_size = match p.get("minSize") {
        None => 7.5,
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite() && (4.0..=72.0).contains(n))
            .ok_or_else(|| bad("story.fitOverset", "`minSize` must be a number between 4 and 72"))?,
    };
    s.edit(|doc, _| {
        let targets: BTreeSet<StoryId> = match explicit_stories {
            Some(sids) => sids,
            None => stories_on_pages(doc, page_range, &[]),
        };
        let (fitted, still_overset) = fit_overset_stories(doc, &targets, min_size);
        Ok(json!({"fitted": fitted, "stillOverset": still_overset}))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diacritics_modes_preserve_formatting_shadda_and_salawat_ligature() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let f = s.execute("frame.create", &json!({"rect": [36, 36, 320, 140], "content": "text", "text": "مُحَمَّدٌ ﷺ رَسُولُ اللهِ"})).unwrap();
        let sid = StoryId(f["story"].as_u64().unwrap());
        let f2 = s.execute("frame.create", &json!({"rect": [36, 160, 320, 240], "content": "text", "text": "كِتَابٌ مُفِيدٌ"})).unwrap();
        let sid2 = StoryId(f2["story"].as_u64().unwrap());
        s.execute("text.select", &json!({"story": sid.0, "anchor": 0, "focus": 12})).unwrap();
        s.execute("type.char", &json!({"attrs": {"size": 18.0}})).unwrap();

        // `keep` mode is a safe no-op matching production schema.
        let kept = s.execute("type.diacritics", &json!({"mode": "keep"})).unwrap();
        assert_eq!(kept["removed"], 0);
        assert!(s.doc().unwrap().doc.stories[&sid].text.contains('\u{064E}'));

        // Out-of-bounds page range must not clamp onto the last page and mutate unrelated stories.
        let oob = s.execute("type.diacritics", &json!({"mode": "none", "pages": [5, 10]})).unwrap();
        assert_eq!(oob["removed"], 0);
        assert!(s.doc().unwrap().doc.stories[&sid].text.contains('\u{064E}'));

        // `scope: "story"` mutates only the selected story (`sid`), leaving `sid2` untouched.
        let scoped = s.execute("type.diacritics", &json!({"mode": "light", "scope": "story"})).unwrap();
        assert!(scoped["removed"].as_u64().unwrap() > 0);
        assert!(s.doc().unwrap().doc.stories[&sid2].text.contains('\u{0650}'), "unselected story untouched");
        let after_light = &s.doc().unwrap().doc.stories[&sid];
        after_light.check().unwrap();
        assert!(after_light.text.contains('\u{0651}'), "light mode keeps shadda");
        assert!(after_light.text.contains('\u{064C}'), "light mode keeps tanween");
        assert!(after_light.text.contains('ﷺ'), "salawat ligature preserved");
        assert!(!after_light.text.contains('\u{064E}'), "fatha stripped");

        s.execute("edit.undo", &json!({})).unwrap();
        assert!(s.doc().unwrap().doc.stories[&sid].text.contains('\u{064E}'));

        s.execute("type.diacritics", &json!({"mode": "none", "stories": [sid.0]})).unwrap();
        let after_none = &s.doc().unwrap().doc.stories[&sid];
        after_none.check().unwrap();
        assert_eq!(after_none.text, "محمد ﷺ رسول الله");
        assert!(s.doc().unwrap().doc.stories[&sid2].text.contains('\u{0650}'), "sid2 still vocalized");
    }

    #[test]
    fn story_fit_overset_binary_searches_large_type_and_scales_explicit_leading() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let long_text = "هذا نص طويل يتجاوز حدود الإطار الصغير عند حجم الخط الكبير ويحتاج إلى تصغير منضبط حتى يتسع بالكامل دون قص.";
        let f = s.execute("frame.create", &json!({"rect": [36, 36, 175, 82], "content": "text", "text": long_text})).unwrap();
        let sid = StoryId(f["story"].as_u64().unwrap());
        s.execute("text.select", &json!({"story": sid.0, "anchor": 0, "focus": s.doc().unwrap().doc.stories[&sid].len()})).unwrap();
        // 30pt size + 38pt explicit leading requires >20pt shrink (far beyond the old 8pt linear cap).
        s.execute("type.char", &json!({"attrs": {"size": 30.0, "leading": {"kind": "points", "value": 38.0}}})).unwrap();
        assert!(s.cache.get(&s.doc().unwrap().doc, sid, None).is_overset());

        let res = s.execute("story.fitOverset", &json!({"story": sid.0, "minSize": 7.5})).unwrap();
        assert_eq!(res["fitted"], 1);
        assert_eq!(res["stillOverset"], 0);
        assert!(!s.cache.get(&s.doc().unwrap().doc, sid, None).is_overset());
        let st = &s.doc().unwrap().doc.stories[&sid];
        let (_, fmt) = st.runs().next().unwrap();
        let fitted_size = fmt.over.size.unwrap();
        assert!((7.5..14.0).contains(&fitted_size), "expected binary-searched size above floor, got {fitted_size}");
        assert!(matches!(fmt.over.leading, Some(designcraft_doc::Leading::Points(l)) if l < 20.0));
    }
}
