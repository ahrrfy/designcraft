//! Interface text bidi & Arabic shaping: read-only galleys (`widget`, `label`, `plain`) and
//! visual-cursor-mapped editable galleys (`editable_layout`, `show_text_edit`) for `egui::TextEdit`.
use egui::text::{CCursor, CCursorRange, CharIndex, LayoutJob, TextWrapping};
use egui::{Align, Align2, Color32, FontFamily, FontId, Galley, Pos2, Rect, WidgetText};
use std::{collections::HashMap, ops::Range, sync::Arc};
use unicode_bidi::{BidiClass, BidiInfo, bidi_class};
use unicode_segmentation::UnicodeSegmentation;

/// Allocation-free preflight: ordinary Latin, CJK and emoji labels need no bidi analysis.
/// Include directional controls so explicit embeddings/overrides keep their existing behavior.
fn may_have_rtl(text: &str) -> bool {
    !text.is_ascii()
        && text.chars().any(|c| {
            matches!(
                bidi_class(c),
                BidiClass::R
                    | BidiClass::AL
                    | BidiClass::AN
                    | BidiClass::LRE
                    | BidiClass::RLE
                    | BidiClass::LRO
                    | BidiClass::RLO
                    | BidiClass::PDF
                    | BidiClass::LRI
                    | BidiClass::RLI
                    | BidiClass::FSI
                    | BidiClass::PDI
            )
        })
}

pub(crate) fn has_rtl(text: &str) -> bool {
    may_have_rtl(text) && BidiInfo::new(text, None).has_rtl()
}

/// Resolve translated labels through bidi, without changing IDs, commands or stored text.
pub fn widget(ui: &egui::Ui, text: impl Into<WidgetText>) -> WidgetText {
    let text = text.into();
    if !has_rtl(text.text()) {
        return text;
    }
    let mut job = (*text.into_layout_job(ui.style(), egui::FontSelection::Default, Align::Center)).clone();
    job.wrap = TextWrapping::from_wrap_mode_and_width(ui.wrap_mode(), ui.available_width());
    WidgetText::Galley(layout(ui.ctx(), job))
}

/// Resolve window titles or headings where only `egui::Context` is available.
pub fn widget_ctx(ctx: &egui::Context, text: impl Into<WidgetText>) -> WidgetText {
    let text = text.into();
    if !has_rtl(text.text()) {
        return text;
    }
    let style = ctx.global_style();
    let job = (*text.into_layout_job(&style, egui::FontSelection::Style(egui::TextStyle::Heading), Align::Center)).clone();
    WidgetText::Galley(layout(ctx, job))
}

/// Labels are read-only: egui's LTR selection indices cannot map bidi glyph order.
/// Accessibility still receives the original logical string through Galley::job.
pub fn label(ui: &mut egui::Ui, text: impl Into<WidgetText>) -> egui::Response {
    let text = text.into();
    let rtl = has_rtl(text.text());
    if !rtl {
        return ui.label(text);
    }
    let selectable = false;
    let rendered = widget(ui, text.clone());
    let elided = matches!(&rendered, WidgetText::Galley(galley) if galley.elided);
    let response = ui.add(egui::Label::new(rendered).selectable(selectable).show_tooltip_when_elided(!rtl));
    if rtl && elided {
        response.on_hover_ui(|ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            label(ui, text);
        })
    } else {
        response
    }
}

/// Isolate a user-provided name when embedding it in LTR chrome/metadata.
/// A Latin product name or suffix must not change the name's internal reading order.
pub fn isolate(text: &str) -> String {
    if text.chars().any(designcraft_fonts::is_rtl) { format!("\u{2068}{text}\u{2069}") } else { text.to_owned() }
}

/// Isolate a technical LTR token (`$0.03`, `8192 MiB / 16384 MiB`, URLs, versions) inside RTL UI copy.
pub fn ltr(text: impl std::fmt::Display) -> String {
    format!("\u{2066}{text}\u{2069}")
}

/// Force RTL paragraph base direction when an Arabic UI line begins with an isolated LTR token.
pub fn rtl_paragraph(text: impl std::fmt::Display) -> String {
    format!("\u{2067}{text}\u{2069}")
}

pub fn plain(ctx: &egui::Context, text: &str, font: FontId, color: Color32) -> Arc<Galley> {
    layout(ctx, LayoutJob::simple_singleline(text.to_owned(), font, color))
}

/// One line no wider than `max_width`: longer text ends in an ellipsis, cut at a grapheme
/// boundary in logical order (`Galley::elided` tells whether it was cut).
pub fn truncated(ctx: &egui::Context, text: &str, font: FontId, color: Color32, max_width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = TextWrapping::truncate_at_width(max_width.max(0.0));
    layout(ctx, job)
}

pub fn paint(p: &egui::Painter, pos: Pos2, anchor: Align2, text: &str, font: FontId, color: Color32) {
    let galley = plain(p.ctx(), text, font, color);
    let rect = anchor.anchor_rect(Rect::from_min_size(pos, galley.size()));
    p.galley(rect.min, galley, color);
}

fn is_bidi_isolate_control(c: char) -> bool {
    matches!(c, '\u{2066}'..='\u{2069}')
}

fn is_zero_width_control_grapheme(gr: &str) -> bool {
    !gr.is_empty() && gr.chars().all(|c| is_bidi_isolate_control(c) || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{FEFF}'))
}

fn is_symbol_fallback(c: char) -> bool {
    matches!(c, '\u{2190}'..='\u{2BFF}' | '\u{1F000}'..='\u{1FAFF}')
}

#[derive(Clone, Debug)]
struct VisualPiece {
    /// Byte subrange within `job.text` covered by this visual piece.
    logical_range: Range<usize>,
    /// Whether this piece flows right-to-left visually.
    rtl: bool,
    /// Index into the returned `LayoutJob::sections`, or `None` when the piece held only isolate controls.
    section_index: Option<usize>,
}

/// Reorder directional runs in visual left-to-right order while recording the exact logical byte
/// subrange and visual section index of every piece so editable layouts can map shaped glyphs back
/// to logical character indices.
fn visual_line_with_pieces(
    job: &LayoutJob,
    bidi: &BidiInfo<'_>,
    line: Range<usize>,
    has_arabic_fonts: (bool, bool),
) -> (LayoutJob, Vec<VisualPiece>) {
    let mut visual = LayoutJob { break_on_newline: false, ..Default::default() };
    let mut mapped_pieces = Vec::new();
    if line.is_empty() {
        if let Some(section) =
            job.sections.iter().find(|s| usize::from(s.byte_range.start) <= line.start && line.start <= usize::from(s.byte_range.end))
        {
            visual.append("", 0.0, section.format.clone());
        }
        return (visual, mapped_pieces);
    }
    let Some(para) = bidi.paragraphs.iter().find(|p| p.range.start <= line.start && line.start < p.range.end) else {
        // Preserve the font/height for an empty line.
        if let Some(section) = job.sections.first() {
            visual.append("", 0.0, section.format.clone());
        }
        return (visual, mapped_pieces);
    };
    let (levels, runs) = bidi.visual_runs(para, line);
    for run in runs {
        let rtl = levels.get(run.start).is_some_and(|lvl| lvl.is_rtl());
        let mut sections: Vec<_> = job
            .sections
            .iter()
            .filter_map(|section| {
                let start = run.start.max(section.byte_range.start.into());
                let end = run.end.min(section.byte_range.end.into());
                (start < end).then_some((section, start..end))
            })
            .collect();
        if rtl {
            sections.reverse();
        }
        for (section, range) in sections {
            let mut format = section.format.clone();
            if rtl {
                let bold = format.font_id.family == FontFamily::Name("semibold".into());
                if bold && has_arabic_fonts.1 {
                    format.font_id.family = FontFamily::Name("arabic-semibold".into());
                    format.coords = egui::epaint::text::VariationCoords::new([(*b"wght", 600.0)]);
                } else if has_arabic_fonts.0 {
                    format.font_id.family = FontFamily::Name("arabic".into());
                } else {
                    format.font_id.family = FontFamily::Proportional;
                }
            }
            let Some(raw) = job.text.get(range.clone()) else {
                continue;
            };
            if rtl && raw.chars().any(is_symbol_fallback) {
                // Emoji/symbol fallback faces split an epaint section in logical left-to-right
                // order; reverse those sub-runs so leading RTL icons stay on the visual right.
                let mut sub_pieces: Vec<(usize, usize, bool)> = Vec::new();
                for (idx, ch) in raw.char_indices() {
                    let end = idx + ch.len_utf8();
                    if is_bidi_isolate_control(ch)
                        && let Some(last) = sub_pieces.last_mut()
                    {
                        last.1 = end;
                        continue;
                    }
                    let sym = is_symbol_fallback(ch);
                    if let Some(last) = sub_pieces.last_mut()
                        && last.2 == sym
                    {
                        last.1 = end;
                    } else {
                        sub_pieces.push((idx, end, sym));
                    }
                }
                let mut lead = section.leading_space;
                for (sub_start, sub_end, sym) in sub_pieces.into_iter().rev() {
                    let logical_sub = (range.start + sub_start)..(range.start + sub_end);
                    let Some(raw_sub) = job.text.get(logical_sub.clone()) else {
                        continue;
                    };
                    let cleaned: std::borrow::Cow<'_, str> = if raw_sub.chars().any(is_bidi_isolate_control) {
                        std::borrow::Cow::Owned(raw_sub.chars().filter(|c| !is_bidi_isolate_control(*c)).collect())
                    } else {
                        std::borrow::Cow::Borrowed(raw_sub)
                    };
                    let sub = cleaned.as_ref();
                    if sub.is_empty() {
                        mapped_pieces.push(VisualPiece { logical_range: logical_sub, rtl: true, section_index: None });
                        continue;
                    }
                    let sec_idx = visual.sections.len();
                    if sym {
                        let mut sym_format = format.clone();
                        sym_format.font_id.family = FontFamily::Proportional;
                        let neutral: String = sub.chars().rev().map(|c| unicode_bidi_mirroring::get_mirrored(c).unwrap_or(c)).collect();
                        visual.append(&neutral, lead, sym_format);
                    } else if !sub.chars().any(designcraft_fonts::is_rtl) {
                        let neutral: String = sub.chars().rev().map(|c| unicode_bidi_mirroring::get_mirrored(c).unwrap_or(c)).collect();
                        visual.append(&neutral, lead, format.clone());
                    } else {
                        visual.append(sub, lead, format.clone());
                    }
                    mapped_pieces.push(VisualPiece { logical_range: logical_sub, rtl: true, section_index: Some(sec_idx) });
                    lead = 0.0;
                }
            } else {
                let cleaned: std::borrow::Cow<'_, str> = if raw.chars().any(is_bidi_isolate_control) {
                    std::borrow::Cow::Owned(raw.chars().filter(|c| !is_bidi_isolate_control(*c)).collect())
                } else {
                    std::borrow::Cow::Borrowed(raw)
                };
                let text = cleaned.as_ref();
                if text.is_empty() {
                    mapped_pieces.push(VisualPiece { logical_range: range, rtl, section_index: None });
                    continue;
                }
                let sec_idx = visual.sections.len();
                if rtl && !text.chars().any(designcraft_fonts::is_rtl) {
                    // Neutral-only RTL runs have no script for the shaper to infer a direction.
                    // Apply UBA L4 mirroring here; Arabic runs are mirrored by the shaper.
                    let neutral: String = text.chars().rev().map(|c| unicode_bidi_mirroring::get_mirrored(c).unwrap_or(c)).collect();
                    visual.append(&neutral, section.leading_space, format);
                } else {
                    visual.append(text, section.leading_space, format);
                }
                mapped_pieces.push(VisualPiece { logical_range: range, rtl, section_index: Some(sec_idx) });
            }
        }
    }
    (visual, mapped_pieces)
}

/// Reorder directional runs, not characters: the shaper needs logical Arabic to join letters.
fn visual_line(job: &LayoutJob, bidi: &BidiInfo<'_>, line: Range<usize>, has_arabic_fonts: (bool, bool)) -> LayoutJob {
    visual_line_with_pieces(job, bidi, line, has_arabic_fonts).0
}

const MAX_CACHED_LABELS: usize = 256;
const MAX_CACHED_TEXT_BYTES: usize = 4096;

#[derive(Clone)]
struct CachedLayout {
    galley: Arc<Galley>,
    last_used: u64,
}

#[derive(Clone, Default)]
struct LayoutCache {
    // A tiny galley owned by egui's font cache acts as a lifetime token. Font changes,
    // DPI changes, atlas rebuilds and eviction replace it. Holding the old Arc prevents
    // address reuse. Unlike a font-definition hash, this also detects atlas resets.
    font_token: Option<Arc<Galley>>,
    entries: HashMap<u64, CachedLayout>,
    clock: u64,
}

impl LayoutCache {
    fn lookup(&mut self, token: Arc<Galley>, key: u64, job: &LayoutJob) -> Option<Arc<Galley>> {
        if self.font_token.as_ref().is_none_or(|old| !Arc::ptr_eq(old, &token)) {
            self.entries.clear();
            self.font_token = Some(token);
        }
        self.clock = self.clock.saturating_add(1);
        let entry = self.entries.get_mut(&key)?;
        // Check equality as well as the hash: collisions must never display another label.
        if *entry.galley.job != *job {
            return None;
        }
        entry.last_used = self.clock;
        Some(Arc::clone(&entry.galley))
    }

    fn insert(&mut self, key: u64, galley: Arc<Galley>) {
        if self.entries.len() >= MAX_CACHED_LABELS
            && let Some(oldest) = self.entries.iter().min_by_key(|(_, entry)| entry.last_used).map(|(key, _)| *key)
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(key, CachedLayout { galley, last_used: self.clock });
    }
}

pub(crate) fn layout(ctx: &egui::Context, job: LayoutJob) -> Arc<Galley> {
    if !may_have_rtl(&job.text) {
        return ctx.fonts_mut(|fonts| fonts.layout_job(job));
    }
    // Keep unusually large/dynamic content from filling a UI-label cache.
    if job.text.len() > MAX_CACHED_TEXT_BYTES || job.sections.len() > 256 {
        return layout_uncached(ctx, job);
    }
    let token = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(String::new(), FontId::default(), Color32::TRANSPARENT));
    let id = egui::Id::new("designcraft.rtl.layout-cache");
    let key = egui::util::hash(&job);
    let cached = ctx.data_mut(|data| data.get_temp_mut_or_default::<LayoutCache>(id).lookup(token, key, &job));
    if let Some(galley) = cached {
        return galley;
    }
    let galley = layout_uncached(ctx, job);
    ctx.data_mut(|data| data.get_temp_mut_or_default::<LayoutCache>(id).insert(key, Arc::clone(&galley)));
    galley
}

#[cfg(test)]
pub(crate) fn matches_rtl_layout(ctx: &egui::Context, galley: &Galley) -> bool {
    let expected = layout_uncached(ctx, (*galley.job).clone());
    let sig = |g: &Galley| -> Vec<(char, u32, u32, u32)> {
        g.rows.iter().flat_map(|r| r.glyphs.iter().map(|gl| (gl.chr, gl.pos.x.to_bits(), gl.pos.y.to_bits(), gl.advance_width.to_bits()))).collect()
    };
    sig(galley) == sig(&expected)
}

fn arabic_font_availability(ctx: &egui::Context) -> (bool, bool) {
    ctx.fonts(|f| {
        let families = f.families();
        (families.contains(&FontFamily::Name("arabic".into())), families.contains(&FontFamily::Name("arabic-semibold".into())))
    })
}

fn layout_uncached(ctx: &egui::Context, mut job: LayoutJob) -> Arc<Galley> {
    if !has_rtl(&job.text) {
        return ctx.fonts_mut(|fonts| fonts.layout_job(job));
    }
    let has_arabic_fonts = arabic_font_availability(ctx);
    let original = Arc::new(job.clone());
    if !job.break_on_newline {
        // Single-line widgets show line separators as spaces; byte offsets stay unchanged.
        job.text = job.text.replace(['\n', '\r'], " ");
    }
    let bidi = BidiInfo::new(&job.text, None);
    let shape = |range| ctx.fonts_mut(|fonts| fonts.layout_job(visual_line(&job, &bidi, range, has_arabic_fonts)));
    // Wrap in logical reading order BEFORE applying bidi separately to each line.
    let mut ranges = Vec::new();
    let mut start = 0;
    for paragraph in job.text.split_inclusive('\n') {
        let end = start + paragraph.len();
        let content_end = if paragraph.ends_with('\n') { end - 1 } else { end };
        if !job.wrap.max_width.is_finite() || shape(start..content_end).size().x <= job.wrap.max_width {
            ranges.push(start..content_end);
            start = end;
            continue;
        }
        let mut line_start = start;
        let mut word_break = start;
        for (offset, cluster) in job.text.get(start..content_end).unwrap_or("").grapheme_indices(true) {
            let index = start + offset;
            let boundary = index + cluster.len();
            if job.wrap.max_width.is_finite() && index > line_start && shape(line_start..boundary).size().x > job.wrap.max_width {
                let cut = if !job.wrap.break_anywhere && word_break > line_start { word_break } else { index };
                ranges.push(line_start..cut);
                line_start = cut;
            }
            if cluster.chars().all(char::is_whitespace) {
                word_break = boundary;
            }
        }
        ranges.push(line_start..content_end);
        start = end;
    }
    if job.text.ends_with('\n') {
        ranges.push(start..start);
    }
    let elided = ranges.len() > job.wrap.max_rows;
    ranges.truncate(job.wrap.max_rows);
    let mut lines = Vec::new();
    for (i, range) in ranges.iter().enumerate() {
        if elided && i + 1 == ranges.len() {
            // Append the ellipsis in logical order, then reorder and shape the final line.
            let mut last = LayoutJob::default();
            for section in &job.sections {
                let start = range.start.max(section.byte_range.start.into());
                let end = range.end.min(section.byte_range.end.into());
                if start < end
                    && let Some(slice) = job.text.get(start..end)
                {
                    last.append(slice, 0.0, section.format.clone());
                }
            }
            let format = last.sections.last().or(job.sections.first()).map(|s| s.format.clone()).unwrap_or_default();
            let tail = job.wrap.overflow_character.map_or(String::new(), |c| c.to_string());
            let base = bidi.paragraphs.iter().find(|p| p.range.contains(&range.start)).map(|p| p.level);
            loop {
                let mut candidate = last.clone();
                candidate.append(&tail, 0.0, format.clone());
                let info = BidiInfo::new(&candidate.text, base);
                let shaped = ctx.fonts_mut(|fonts| fonts.layout_job(visual_line(&candidate, &info, 0..candidate.text.len(), has_arabic_fonts)));
                if shaped.size().x <= job.wrap.max_width || last.text.is_empty() {
                    lines.push(shaped);
                    break;
                }
                let end = last.text.grapheme_indices(true).next_back().map_or(0, |(i, _)| i);
                last.text.truncate(end);
                for section in &mut last.sections {
                    section.byte_range.end = usize::from(section.byte_range.end).min(end).into();
                }
                last.sections.retain(|s| s.byte_range.start < s.byte_range.end);
            }
        } else {
            lines.push(shape(range.clone()));
        }
    }
    let mut galley = Galley::concat(original, &lines, ctx.pixels_per_point());
    galley.elided = elided;
    // Each Arabic line shares the right edge; Latin runs and numbers remain LTR.
    let width = galley.rect.width();
    galley.mesh_bounds = Rect::NOTHING;
    for row in &mut galley.rows {
        row.pos.x += width - row.size.x;
        galley.mesh_bounds |= row.visuals.mesh_bounds.translate(row.pos.to_vec2());
    }
    Arc::new(galley)
}

/// Partition `shaped_glyphs` into slices corresponding 1-to-1 with `visual_job.sections`.
fn section_glyph_ranges(ctx: &egui::Context, visual_job: &LayoutJob, total_glyphs: usize) -> Vec<Range<usize>> {
    let sec_count = visual_job.sections.len();
    if sec_count == 0 {
        return Vec::new();
    }
    if sec_count == 1 {
        return std::iter::once(0..total_glyphs).collect();
    }
    let mut out = Vec::with_capacity(sec_count);
    let mut cursor = 0usize;
    for (idx, sec) in visual_job.sections.iter().enumerate() {
        if idx + 1 == sec_count {
            out.push(cursor..total_glyphs);
            break;
        }
        let slice = visual_job.text.get(usize::from(sec.byte_range.start)..usize::from(sec.byte_range.end)).unwrap_or("");
        let mut single = LayoutJob { break_on_newline: false, ..Default::default() };
        single.append(slice, 0.0, sec.format.clone());
        let len = ctx.fonts_mut(|fonts| fonts.layout_job(single)).rows.first().map_or(0, |r| r.glyphs.len());
        let next = cursor.saturating_add(len).min(total_glyphs);
        out.push(cursor..next);
        cursor = next;
    }
    out
}

#[derive(Clone, Copy)]
struct ShapedCluster {
    chr: char,
    lx: f32,
    rx: f32,
}

fn subdivide_span(seg_lx: f32, seg_rx: f32, count: usize, rtl: bool) -> Vec<(f32, f32)> {
    if count == 0 {
        return Vec::new();
    }
    let step = (seg_rx - seg_lx).max(0.0) / (count as f32);
    (0..count)
        .map(|i| {
            if rtl {
                let r = if i == 0 { seg_rx } else { seg_rx - (i as f32) * step };
                let l = if i + 1 == count { seg_lx } else { seg_rx - ((i + 1) as f32) * step };
                (l, r)
            } else {
                let l = if i == 0 { seg_lx } else { seg_lx + (i as f32) * step };
                let r = if i + 1 == count { seg_rx } else { seg_lx + ((i + 1) as f32) * step };
                (l, r)
            }
        })
        .collect()
}

fn map_segment_spans(g_chars: &[char], clusters: &[ShapedCluster], rtl: bool, fallback_lx: f32, fallback_rx: f32) -> Vec<(f32, f32)> {
    let wg = g_chars.len();
    let wc = clusters.len();
    if wg == 0 {
        return Vec::new();
    }
    if wg == wc {
        return clusters.iter().map(|c| (c.lx, c.rx)).collect();
    }
    if wg > wc && wc > 0 {
        let mut spans = vec![(0.0_f32, 0.0_f32); wg];
        let (mut g_lo, mut g_hi) = (0usize, wg);
        let (mut c_lo, mut c_hi) = (0usize, wc);
        while (c_hi - c_lo) > 1 && (g_hi - g_lo) > 1 && g_chars.get(g_hi - 1) == clusters.get(c_hi - 1).map(|c| &c.chr) {
            if let (Some(dst), Some(src)) = (spans.get_mut(g_hi - 1), clusters.get(c_hi - 1)) {
                *dst = (src.lx, src.rx);
            }
            g_hi -= 1;
            c_hi -= 1;
        }
        while (c_hi - c_lo) > 1 && (g_hi - g_lo) > 1 && g_chars.get(g_lo) == clusters.get(c_lo).map(|c| &c.chr) {
            if let (Some(dst), Some(src)) = (spans.get_mut(g_lo), clusters.get(c_lo)) {
                *dst = (src.lx, src.rx);
            }
            g_lo += 1;
            c_lo += 1;
        }
        let rem_g = g_hi - g_lo;
        let rem_c = c_hi - c_lo;
        let mid_slice = clusters.get(c_lo..c_hi).unwrap_or(&[]);
        if rem_g == rem_c {
            for (offset, src) in mid_slice.iter().enumerate() {
                if let Some(dst) = spans.get_mut(g_lo + offset) {
                    *dst = (src.lx, src.rx);
                }
            }
        } else {
            let seg_lx = mid_slice.iter().map(|c| c.lx).fold(fallback_lx, f32::min);
            let seg_rx = mid_slice.iter().map(|c| c.rx).fold(fallback_rx, f32::max);
            for (offset, sp) in subdivide_span(seg_lx, seg_rx, rem_g, rtl).into_iter().enumerate() {
                if let Some(dst) = spans.get_mut(g_lo + offset) {
                    *dst = sp;
                }
            }
        }
        return spans;
    }
    let (seg_lx, seg_rx) = if wc > 0 {
        let lx = clusters.iter().map(|c| c.lx).fold(f32::INFINITY, f32::min);
        let rx = clusters.iter().map(|c| c.rx).fold(f32::NEG_INFINITY, f32::max);
        (lx, rx.max(lx))
    } else {
        (fallback_lx, fallback_rx.max(fallback_lx))
    };
    subdivide_span(seg_lx, seg_rx, wg, rtl)
}

/// Map the shaped visual glyphs of one BiDi piece back onto its logical characters:
/// - Adjacent shaped clusters form a watertight visual partition (`right_k == left_{k+1}`), so
///   combining marks (`advance_width = 0.0`) sit on the exact inter-cluster boundary with zero jump.
/// - Multi-word runs split on ASCII spaces so Arabic ligatures (`لا`, `اللَّهِ`, `اللازم`) subdivide
///   only the ligated cluster(s) inside that word while every unligated letter keeps its exact
///   shaped visual width.
fn map_piece_to_logical_glyphs(
    piece_str: &str,
    rtl: bool,
    sec_glyphs: &[egui::epaint::text::Glyph],
    fallback_x: f32,
    next_bound_x: Option<f32>,
    tmpl: egui::epaint::text::Glyph,
    end_vtx: u32,
) -> (Vec<egui::epaint::text::Glyph>, f32) {
    let pos_glyphs: Vec<&egui::epaint::text::Glyph> = sec_glyphs.iter().filter(|g| g.advance_width > 0.0).collect();
    let mut pos_clusters = Vec::with_capacity(pos_glyphs.len());
    for (k, g) in pos_glyphs.iter().enumerate() {
        let lx = g.pos.x;
        let rx = if let Some(next_g) = pos_glyphs.get(k + 1) {
            next_g.pos.x.max(lx)
        } else if let Some(bound) = next_bound_x {
            bound.max(lx)
        } else {
            (g.pos.x + g.advance_width).max(lx)
        };
        pos_clusters.push(ShapedCluster { chr: g.chr, lx, rx });
    }
    let piece_left = pos_clusters.first().map_or_else(|| sec_glyphs.first().map_or(fallback_x, |g| g.pos.x), |c| c.lx);
    let piece_right = pos_clusters.last().map_or(next_bound_x.unwrap_or(piece_left).max(piece_left), |c| c.rx);

    let graphemes: Vec<&str> = piece_str.graphemes(true).collect();
    let vis_indices: Vec<usize> = graphemes.iter().enumerate().filter_map(|(i, gr)| (!is_zero_width_control_grapheme(gr)).then_some(i)).collect();
    let g_chars: Vec<char> = vis_indices.iter().map(|&idx| graphemes.get(idx).and_then(|gr| gr.chars().next()).unwrap_or('\0')).collect();

    let logical_clusters: Vec<ShapedCluster> = if rtl { pos_clusters.into_iter().rev().collect() } else { pos_clusters };

    let g_spaces: Vec<usize> = g_chars.iter().enumerate().filter_map(|(i, &ch)| (ch == ' ').then_some(i)).collect();
    let c_spaces: Vec<usize> = logical_clusters.iter().enumerate().filter_map(|(i, c)| (c.chr == ' ').then_some(i)).collect();

    let vis_spans: Vec<(f32, f32)> = if !g_spaces.is_empty() && g_spaces.len() == c_spaces.len() {
        let mut all = Vec::with_capacity(g_chars.len());
        let mut g_prev = 0usize;
        let mut c_prev = 0usize;
        for (&gs, &cs) in g_spaces.iter().zip(c_spaces.iter()) {
            let word_g = g_chars.get(g_prev..gs).unwrap_or(&[]);
            let word_c = logical_clusters.get(c_prev..cs).unwrap_or(&[]);
            let sp_c = logical_clusters.get(cs).copied().unwrap_or(ShapedCluster { chr: ' ', lx: piece_left, rx: piece_right });
            let word_fb = if rtl { sp_c.rx } else { sp_c.lx };
            all.extend(map_segment_spans(word_g, word_c, rtl, word_fb, word_fb));
            all.push((sp_c.lx, sp_c.rx));
            g_prev = gs + 1;
            c_prev = cs + 1;
        }
        let tail_g = g_chars.get(g_prev..).unwrap_or(&[]);
        let tail_c = logical_clusters.get(c_prev..).unwrap_or(&[]);
        let tail_fb = if rtl { piece_left } else { piece_right };
        all.extend(map_segment_spans(tail_g, tail_c, rtl, tail_fb, tail_fb));
        all
    } else {
        map_segment_spans(&g_chars, &logical_clusters, rtl, piece_left, piece_right)
    };

    let mut out = Vec::with_capacity(piece_str.chars().count());
    let mut cursor_x = if rtl { piece_right } else { piece_left };
    let mut vis_cursor = 0usize;

    for gr in graphemes {
        if is_zero_width_control_grapheme(gr) {
            for ch in gr.chars() {
                let mut g = tmpl;
                g.chr = ch;
                g.pos.x = cursor_x;
                g.advance_width = 0.0;
                g.uv_rect = Default::default();
                g.first_vertex = end_vtx;
                out.push(g);
            }
            continue;
        }
        let (lx, rx) = vis_spans.get(vis_cursor).copied().unwrap_or((cursor_x, cursor_x));
        vis_cursor += 1;
        let w = (rx - lx).max(0.0);
        let mut chars = gr.chars();
        if let Some(base_ch) = chars.next() {
            let mut g = tmpl;
            g.chr = base_ch;
            if rtl {
                g.pos.x = rx;
                g.advance_width = -w;
            } else {
                g.pos.x = lx;
                g.advance_width = w;
            }
            g.uv_rect = Default::default();
            g.first_vertex = end_vtx;
            out.push(g);
        }
        let trailing_x = if rtl { lx } else { rx };
        for mark_ch in chars {
            let mut g = tmpl;
            g.chr = mark_ch;
            g.pos.x = trailing_x;
            g.advance_width = 0.0;
            g.uv_rect = Default::default();
            g.first_vertex = end_vtx;
            out.push(g);
        }
        cursor_x = trailing_x;
    }

    (out, piece_right)
}

/// Lay out text for an editable [`egui::TextEdit`] while preserving visual RTL run/word order,
/// exact 1-glyph-per-`char` counts, and real visual glyph positions for logical cursors.
pub fn editable_layout(ui: &egui::Ui, text: &str, wrap_width: f32, font_id: FontId, text_color: Color32, multiline: bool) -> Arc<Galley> {
    if !has_rtl(text) {
        let job = if multiline {
            LayoutJob::simple(text.to_owned(), font_id, text_color, wrap_width)
        } else {
            LayoutJob::simple_singleline(text.to_owned(), font_id, text_color)
        };
        return ui.fonts_mut(|fonts| fonts.layout_job(job));
    }
    let mut job = if multiline {
        LayoutJob::simple(text.to_owned(), font_id.clone(), text_color, wrap_width)
    } else {
        LayoutJob::simple_singleline(text.to_owned(), font_id.clone(), text_color)
    };
    let original = Arc::new(job.clone());
    if !job.break_on_newline {
        job.text = job.text.replace(['\n', '\r'], " ");
    }
    let ctx = ui.ctx();
    let has_arabic_fonts = arabic_font_availability(ctx);
    let bidi = BidiInfo::new(&job.text, None);
    let shape = |range: Range<usize>| ctx.fonts_mut(|fonts| fonts.layout_job(visual_line(&job, &bidi, range, has_arabic_fonts)));
    let mut ranges: Vec<(Range<usize>, bool)> = Vec::new();
    let mut start = 0usize;
    for paragraph in job.text.split_inclusive('\n') {
        let end = start + paragraph.len();
        let has_nl = paragraph.ends_with('\n');
        let content_end = if has_nl { end - 1 } else { end };
        if !job.wrap.max_width.is_finite() || shape(start..content_end).size().x <= job.wrap.max_width {
            ranges.push((start..content_end, has_nl));
            start = end;
            continue;
        }
        let mut line_start = start;
        let mut word_break = start;
        for (offset, cluster) in job.text.get(start..content_end).unwrap_or("").grapheme_indices(true) {
            let index = start + offset;
            let boundary = index + cluster.len();
            if job.wrap.max_width.is_finite() && index > line_start && shape(line_start..boundary).size().x > job.wrap.max_width {
                let cut = if !job.wrap.break_anywhere && word_break > line_start { word_break } else { index };
                ranges.push((line_start..cut, false));
                line_start = cut;
            }
            if cluster.chars().all(char::is_whitespace) {
                word_break = boundary;
            }
        }
        ranges.push((line_start..content_end, has_nl));
        start = end;
    }
    if job.text.ends_with('\n') {
        ranges.push((start..start, false));
    }

    let mut line_meta: Vec<(LayoutJob, Vec<VisualPiece>)> = Vec::with_capacity(ranges.len());
    let mut lines: Vec<Arc<Galley>> = Vec::with_capacity(ranges.len());
    for (range, _) in &ranges {
        let (visual_job, pieces) = visual_line_with_pieces(&job, &bidi, range.clone(), has_arabic_fonts);
        let shaped = ctx.fonts_mut(|fonts| fonts.layout_job(visual_job.clone()));
        line_meta.push((visual_job, pieces));
        lines.push(shaped);
    }

    let mut galley = Galley::concat(original, &lines, ctx.pixels_per_point());
    let fallback_glyph = galley.rows.iter().find_map(|r| r.glyphs.first().copied()).or_else(|| {
        ctx.fonts_mut(|fonts| fonts.layout_no_wrap(" ".to_owned(), font_id, text_color)).rows.first().and_then(|r| r.glyphs.first().copied())
    });
    let width = galley.rect.width();
    galley.mesh_bounds = Rect::NOTHING;
    for ((placed, (range, hard_nl)), (visual_job, pieces)) in galley.rows.iter_mut().zip(ranges.iter()).zip(line_meta.iter()) {
        placed.pos.x += width - placed.size.x;
        placed.ends_with_newline = *hard_nl;
        galley.mesh_bounds |= placed.visuals.mesh_bounds.translate(placed.pos.to_vec2());
        let slice = job.text.get(range.clone()).unwrap_or("");
        let chars: Vec<(usize, char)> = slice.char_indices().collect();
        let row = Arc::make_mut(&mut placed.row);
        if chars.is_empty() {
            row.glyphs.clear();
        } else if let Some(tmpl) = row.glyphs.first().copied().or(fallback_glyph) {
            let shaped_glyphs = row.glyphs.clone();
            let sec_ranges = section_glyph_ranges(ctx, visual_job, shaped_glyphs.len());
            let end_vtx = u32::try_from(row.visuals.glyph_vertex_range.end).unwrap_or(u32::MAX);
            let mut byte_to_char = HashMap::with_capacity(chars.len() + 1);
            for (ci, (byte_off, _)) in chars.iter().enumerate() {
                byte_to_char.insert(range.start + *byte_off, ci);
            }
            byte_to_char.insert(range.end, chars.len());

            let mut slots: Vec<Option<egui::epaint::text::Glyph>> = vec![None; chars.len()];
            let mut running_visual_x = 0.0_f32;
            for (pi, piece) in pieces.iter().enumerate() {
                let sec_slice = piece.section_index.and_then(|si| sec_ranges.get(si)).and_then(|r| shaped_glyphs.get(r.clone())).unwrap_or(&[]);
                let next_bound_x = pieces
                    .get(pi + 1..)
                    .unwrap_or(&[])
                    .iter()
                    .find_map(|np| {
                        let ns = np.section_index.and_then(|si| sec_ranges.get(si)).and_then(|r| shaped_glyphs.get(r.clone()))?;
                        ns.iter().find(|g| g.advance_width > 0.0).or_else(|| ns.first()).map(|g| g.pos.x)
                    })
                    .or_else(|| (!sec_slice.is_empty()).then_some(row.size.x));
                let piece_tmpl = sec_slice.first().copied().unwrap_or(tmpl);
                let piece_str = job.text.get(piece.logical_range.clone()).unwrap_or("");
                let (mapped, next_x) =
                    map_piece_to_logical_glyphs(piece_str, piece.rtl, sec_slice, running_visual_x, next_bound_x, piece_tmpl, end_vtx);
                running_visual_x = next_x;
                if let Some(&char_start) = byte_to_char.get(&piece.logical_range.start) {
                    for (offset, glyph) in mapped.into_iter().enumerate() {
                        if let Some(slot) = slots.get_mut(char_start + offset) {
                            *slot = Some(glyph);
                        }
                    }
                }
            }

            let mut normalized = Vec::with_capacity(chars.len());
            let mut last_x = row.size.x;
            for ((_, ch), slot) in chars.into_iter().zip(slots) {
                if let Some(g) = slot {
                    last_x = g.pos.x + g.advance_width;
                    normalized.push(g);
                } else {
                    let mut g = tmpl;
                    g.chr = ch;
                    g.pos.x = last_x;
                    g.advance_width = 0.0;
                    g.uv_rect = Default::default();
                    g.first_vertex = end_vtx;
                    normalized.push(g);
                }
            }
            row.glyphs = normalized;
        }
    }
    Arc::new(galley)
}

pub fn text_edit_layouter(
    multiline: bool,
    font_override: Option<FontId>,
    color_override: Option<Color32>,
) -> impl FnMut(&egui::Ui, &dyn egui::TextBuffer, f32) -> Arc<Galley> {
    move |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width: f32| {
        let font_id = font_override.clone().unwrap_or_else(|| egui::TextStyle::Body.resolve(ui.style()));
        let text_color = color_override.unwrap_or_else(|| ui.visuals().text_color());
        editable_layout(ui, text.as_str(), wrap_width, font_id, text_color, multiline)
    }
}

/// Hit-test a galley-local coordinate against real BiDi visual glyph bounds, returning
/// `(insertion_caret_cursor, word_or_line_anchor_cursor)`.
pub(crate) fn bidi_hit_pos(galley: &Galley, pos: egui::Vec2) -> (CCursor, CCursor) {
    if !has_rtl(galley.text()) {
        let c = galley.cursor_from_pos(pos);
        return (c, c);
    }
    const VMARGIN: f32 = 5.0;
    if let Some(first_row) = galley.rows.first()
        && pos.y < first_row.min_y() - VMARGIN
    {
        let b = galley.begin();
        return (b, b);
    }
    if let Some(last_row) = galley.rows.last()
        && last_row.max_y() + VMARGIN < pos.y
    {
        let e = galley.end();
        return (e, e);
    }

    let mut best_y_dist = f32::INFINITY;
    let mut best = (CCursor::default(), CCursor::default());
    let mut row_char_start = 0usize;

    for placed in &galley.rows {
        let min_y = placed.min_y();
        let max_y = placed.max_y();
        let within_y = min_y <= pos.y && pos.y <= max_y;
        let y_dist = (min_y - pos.y).abs().min((max_y - pos.y).abs());
        if within_y || y_dist < best_y_dist {
            best_y_dist = y_dist;
            let rel_x = pos.x - placed.pos.x;
            let glyphs = &placed.row.glyphs;
            let row_len = glyphs.len();

            // Group each base glyph with its trailing zero-advance combining/control glyphs.
            let mut clusters: Vec<(usize, usize, f32, f32, bool)> = Vec::new();
            let mut idx = 0usize;
            while let Some(g) = glyphs.get(idx) {
                if g.advance_width == 0.0 {
                    idx += 1;
                    continue;
                }
                let rtl = g.advance_width < 0.0;
                let vis_left = g.pos.x.min(g.max_x());
                let vis_right = g.pos.x.max(g.max_x());
                let mut end = idx + 1;
                while let Some(next) = glyphs.get(end) {
                    if next.advance_width == 0.0 {
                        end += 1;
                    } else {
                        break;
                    }
                }
                clusters.push((idx, end, vis_left, vis_right, rtl));
                idx = end;
            }

            best = if let Some(&(start_col, end_col, vis_left, vis_right, rtl)) = clusters.iter().min_by(|a, b| {
                let da = if rel_x < a.2 {
                    a.2 - rel_x
                } else if rel_x > a.3 {
                    rel_x - a.3
                } else {
                    0.0
                };
                let db = if rel_x < b.2 {
                    b.2 - rel_x
                } else if rel_x > b.3 {
                    rel_x - b.3
                } else {
                    0.0
                };
                da.total_cmp(&db)
            }) {
                let mid = 0.5 * (vis_left + vis_right);
                let caret_col = if rtl {
                    if rel_x > mid { start_col } else { end_col }
                } else if rel_x < mid {
                    start_col
                } else {
                    end_col
                };
                let prefer_next_row = caret_col < row_len;
                (
                    CCursor { index: CharIndex(row_char_start + caret_col), prefer_next_row },
                    CCursor { index: CharIndex(row_char_start + start_col), prefer_next_row: true },
                )
            } else {
                let c = CCursor { index: CharIndex(row_char_start), prefer_next_row: true };
                (c, c)
            };

            if within_y {
                return best;
            }
        }
        row_char_start += placed.char_count_including_newline().0;
    }

    best
}

/// Cursor at the given galley-local position, aware of RTL and mixed-BiDi glyph positions.
#[cfg(test)]
fn bidi_cursor_from_pos(galley: &Galley, pos: egui::Vec2) -> CCursor {
    bidi_hit_pos(galley, pos).0
}

fn select_word_range(text: &str, anchor: CCursor) -> CCursorRange {
    if text.is_empty() {
        return CCursorRange::one(anchor);
    }
    let target = anchor.index.0;
    let mut line_char_start = 0usize;
    for line in text.split_inclusive('\n') {
        let has_nl = line.ends_with('\n');
        let content = if has_nl { &line[..line.len() - 1] } else { line };
        let content_chars = content.chars().count();
        let line_char_end = line_char_start + content_chars;
        if target <= line_char_end || !has_nl {
            let mut cur = line_char_start;
            for (_, word) in content.split_word_bound_indices() {
                let wlen = word.chars().count();
                let wend = cur + wlen;
                if target < wend {
                    return CCursorRange::two(CCursor::new(cur), CCursor::new(wend));
                }
                cur = wend;
            }
            return CCursorRange::one(CCursor::new(line_char_end));
        }
        line_char_start = line_char_end + usize::from(has_nl);
    }
    CCursorRange::one(anchor)
}

fn select_line_range(text: &str, anchor: CCursor) -> CCursorRange {
    if text.is_empty() {
        return CCursorRange::one(anchor);
    }
    let target = anchor.index.0;
    let mut line_char_start = 0usize;
    for line in text.split_inclusive('\n') {
        let line_chars = line.chars().count();
        let line_char_end = line_char_start + line_chars;
        if target < line_char_end {
            return CCursorRange::two(CCursor::new(line_char_start), CCursor::new(line_char_end));
        }
        line_char_start = line_char_end;
    }
    CCursorRange::two(CCursor::new(line_char_start), CCursor::new(text.chars().count()))
}

fn apply_bidi_pointer_interaction(ui: &egui::Ui, out: &mut egui::text_edit::TextEditOutput) {
    if !has_rtl(out.galley.text()) {
        return;
    }
    let Some(pointer_pos) = out.response.interact_pointer_pos() else {
        return;
    };
    let painted_origin = out.galley_pos - egui::vec2(out.galley.rect.left(), 0.0);
    let local_pos = pointer_pos - painted_origin;
    let (caret_cursor, word_cursor) = bidi_hit_pos(&out.galley, local_pos);

    let new_range = if out.response.double_clicked() {
        Some(select_word_range(out.galley.text(), word_cursor))
    } else if out.response.triple_clicked() {
        Some(select_line_range(out.galley.text(), word_cursor))
    } else if out.response.sense.senses_drag() {
        if out.response.hovered() && ui.input(|i| i.pointer.any_pressed()) {
            if ui.input(|i| i.modifiers.shift) {
                if let Some(mut existing) = out.state.cursor.range(&out.galley) {
                    existing.primary = caret_cursor;
                    Some(existing)
                } else {
                    Some(CCursorRange::one(caret_cursor))
                }
            } else {
                Some(CCursorRange::one(caret_cursor))
            }
        } else if ui.is_being_dragged(out.response.id) {
            out.state.cursor.range(&out.galley).map(|mut existing| {
                existing.primary = caret_cursor;
                existing
            })
        } else {
            None
        }
    } else {
        None
    };

    if let Some(range) = new_range {
        if out.state.cursor.char_range() != Some(range) {
            out.state.cursor.set_char_range(Some(range));
            out.state.clone().store(ui.ctx(), out.response.id);
            ui.ctx().request_repaint();
        }
        out.cursor_range = out.state.cursor.range(&out.galley);
    }
}

/// Show a [`egui::TextEdit`] while mapping pointer clicks, drags, and word selections onto true
/// visual BiDi glyph bounds when RTL text is present.
pub fn show_text_edit(ui: &mut egui::Ui, edit: egui::TextEdit<'_>) -> egui::text_edit::TextEditOutput {
    let mut out = edit.show(ui);
    apply_bidi_pointer_interaction(ui, &mut out);
    out
}

/// Place a [`egui::TextEdit`] inside `max_rect` (like [`egui::Ui::put`]) with BiDi pointer cursor mapping.
pub fn put_text_edit(ui: &mut egui::Ui, max_rect: Rect, edit: egui::TextEdit<'_>) -> egui::Response {
    ui.scope_builder(egui::UiBuilder::new().max_rect(max_rect), |ui| show_text_edit(ui, edit)).inner.response.response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_fonts(test: impl FnOnce(&egui::Context)) {
        let ctx = egui::Context::default();
        // The built-in faces only: installed fonts (the CJK fallbacks) would make the glyphs
        // measured here depend on the machine.
        ctx.set_fonts(crate::theme::font_definitions(designcraft_fonts::CRAFT_FONTS, "ar"));
        ctx.begin_pass(egui::RawInput { max_texture_side: Some(8192), ..Default::default() });
        test(&ctx);
        let mut output = ctx.end_pass();
        output.textures_delta.clear();
    }

    fn displayed(galley: &Galley) -> String {
        galley.rows.iter().flat_map(|row| row.glyphs.iter().filter(|g| g.advance_width > 0.0).map(|g| g.chr)).collect()
    }

    fn cache_job() -> LayoutJob {
        LayoutJob::simple("ملف (PDF 123) جديد للنشر والطباعة".repeat(4), FontId::proportional(15.0), Color32::WHITE, 150.0)
    }

    #[test]
    fn cache_reuses_final_layout_across_frames_and_matches_uncached_output() {
        with_fonts(|ctx| {
            let job = cache_job();
            let first = layout(ctx, job.clone());
            let reference = layout_uncached(ctx, job.clone());
            assert_eq!(displayed(&first), displayed(&reference));
            assert_eq!(first.rect, reference.rect);
            assert_eq!(first.rows.len(), reference.rows.len());
            assert!(Arc::ptr_eq(&first, &layout(ctx, job.clone())));
            ctx.end_pass().textures_delta.clear();
            ctx.begin_pass(egui::RawInput { max_texture_side: Some(8192), ..Default::default() });
            assert!(Arc::ptr_eq(&first, &layout(ctx, job)));
        });
    }

    #[test]
    fn cache_respects_text_formatting_wrapping_and_elision() {
        with_fonts(|ctx| {
            let job = cache_job();
            let first = layout(ctx, job.clone());
            let mut variants = Vec::new();
            let mut changed = job.clone();
            changed.wrap.max_width = 80.0;
            variants.push(changed);
            let mut changed = job.clone();
            changed.wrap.max_rows = 1;
            variants.push(changed);
            let mut changed = job.clone();
            changed.sections[0].format.font_id.size = 24.0;
            variants.push(changed);
            let mut changed = job.clone();
            changed.sections[0].format.color = Color32::RED;
            variants.push(changed);
            variants.push(LayoutJob::simple_singleline("עברית 123".into(), FontId::default(), Color32::WHITE));
            for changed in variants {
                let cached = layout(ctx, changed.clone());
                let reference = layout_uncached(ctx, changed);
                assert!(!Arc::ptr_eq(&first, &cached));
                assert_eq!(cached.job, reference.job);
                assert_eq!(cached.rect, reference.rect);
                assert_eq!(cached.elided, reference.elided);
                assert_eq!(displayed(&cached), displayed(&reference));
            }
        });
    }

    #[test]
    fn cache_invalidates_when_fonts_or_scale_change() {
        with_fonts(|ctx| {
            let job = cache_job();
            let first = layout(ctx, job.clone());
            ctx.end_pass().textures_delta.clear();
            ctx.set_pixels_per_point(2.0);
            ctx.begin_pass(egui::RawInput { max_texture_side: Some(8192), ..Default::default() });
            let scaled = layout(ctx, job.clone());
            assert!(!Arc::ptr_eq(&first, &scaled));
            ctx.end_pass().textures_delta.clear();
            let mut fonts = ctx.fonts(|fonts| fonts.definitions().clone());
            fonts.families.insert(FontFamily::Name("cache-test".into()), Vec::new());
            ctx.set_fonts(fonts);
            ctx.begin_pass(egui::RawInput { max_texture_side: Some(8192), ..Default::default() });
            let new_fonts = layout(ctx, job.clone());
            assert!(!Arc::ptr_eq(&scaled, &new_fonts));
            ctx.end_pass().textures_delta.clear();
            // Changing atlas options recreates the atlas without changing font definitions.
            ctx.begin_pass(egui::RawInput { max_texture_side: Some(4096), ..Default::default() });
            let new_atlas = layout(ctx, job.clone());
            assert!(!Arc::ptr_eq(&new_fonts, &new_atlas));
            assert_eq!(displayed(&new_atlas), displayed(&layout_uncached(ctx, job)));
        });
    }

    #[test]
    fn cache_is_bounded_and_font_token_replacement_invalidates_entries() {
        with_fonts(|ctx| {
            let job = cache_job();
            let galley = layout_uncached(ctx, job.clone());
            let token = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(String::new(), FontId::default(), Color32::TRANSPARENT));
            let mut cache = LayoutCache::default();
            cache.lookup(token, 0, &job);
            for key in 0..=MAX_CACHED_LABELS as u64 {
                cache.clock += 1;
                cache.insert(key, Arc::clone(&galley));
            }
            assert_eq!(cache.entries.len(), MAX_CACHED_LABELS);
            assert!(!cache.entries.contains_key(&0));
            // Replacing the font-cache token models atlas recreation, even with identical fonts.
            let replacement = Arc::new((*galley).clone());
            assert!(cache.lookup(replacement, 1, &job).is_none());
            assert!(cache.entries.is_empty());
        });
    }

    #[test]
    #[ignore = "manual timing comparison, no machine-dependent pass threshold"]
    fn benchmark_cached_wrapping() {
        with_fonts(|ctx| {
            let job = cache_job();
            let _ = layout(ctx, job.clone());
            let start = std::time::Instant::now();
            for _ in 0..200 {
                std::hint::black_box(layout_uncached(ctx, job.clone()));
            }
            let uncached = start.elapsed();
            let start = std::time::Instant::now();
            for _ in 0..200 {
                std::hint::black_box(layout(ctx, job.clone()));
            }
            eprintln!("200 wrapped labels: uncached={uncached:?}, cached={:?}", start.elapsed());
        });
    }

    #[test]
    fn ordinary_multilingual_labels_skip_bidi_analysis() {
        for text in ["", "File (123)", "Édition", "中文排版", "スウォッチ", "e\u{301}", "🎨 🙂"] {
            assert!(!may_have_rtl(text), "{text:?}");
            assert!(!BidiInfo::new(text, None).has_rtl(), "{text:?}");
        }
        for text in ["ملف", "עברית", "PDF ملف 123", "\u{1e900}", "\u{200f}", "\u{202e}abc\u{202c}", "\u{2067}abc\u{2069}", "١٢٣"] {
            assert!(may_have_rtl(text), "{text:?}");
            assert_eq!(has_rtl(text), BidiInfo::new(text, None).has_rtl(), "{text:?}");
        }
    }

    #[test]
    fn non_rtl_widgets_keep_native_text_and_formatting() {
        with_fonts(|ctx| {
            egui::Area::new(egui::Id::new("rtl-fast-path-test")).show(ctx, |ui| {
                for text in ["中文", "日本語", "Édition", "🎨"] {
                    let rendered = widget(ui, egui::RichText::new(text).strong());
                    assert!(matches!(rendered, WidgetText::RichText(_)));
                    assert_eq!(rendered.text(), text);
                    assert!(label(ui, text).rect.is_finite());
                }
            });
        });
    }

    #[test]
    fn mixed_arabic_latin_digits_and_brackets_preserve_logical_source() {
        with_fonts(|ctx| {
            let text = "ملف (PDF 123) جديد";
            let galley = plain(ctx, text, FontId::proportional(15.0), Color32::WHITE);
            assert_eq!(galley.job.text, text);
            let visual = displayed(&galley);
            assert!(visual.contains("PDF 123"), "{visual}");
            let row = &galley.rows[0];
            let left_bracket = row.glyphs.iter().find(|g| g.chr == ')' && !g.uv_rect.is_nothing()).unwrap();
            // A neutral-only final bracket must be mirrored even without an Arabic script run.
            let job = LayoutJob::simple_singleline("ملف (PDF 123)".to_owned(), FontId::proportional(15.0), Color32::WHITE);
            let bidi = BidiInfo::new(&job.text, None);
            let visual_job = visual_line(&job, &bidi, 0..job.text.len(), (true, true));
            assert!(visual_job.text.starts_with("(PDF 123"), "{}", visual_job.text);
            let p = row.glyphs.iter().find(|g| g.chr == 'P').unwrap();
            let three = row.glyphs.iter().find(|g| g.chr == '3').unwrap();
            assert!(left_bracket.pos.x < p.pos.x && p.pos.x < three.pos.x);
            assert!(visual.starts_with('د'), "Arabic final word belongs at the left: {visual}");
            assert!(galley.rect.is_finite());
            assert!(galley.mesh_bounds.is_finite());
        });
    }

    #[test]
    fn isolates_arabic_names_from_latin_brand_and_zoom_suffix() {
        with_fonts(|ctx| {
            for text in [format!("DesignCraft — {}", isolate("ملف (PDF 123) جديد")), format!("{} @ 89%", isolate("ملف (PDF 123) جديد"))]
            {
                let galley = plain(ctx, &text, FontId::proportional(15.0), Color32::WHITE);
                let glyphs = &galley.rows[0].glyphs;
                let x = |c| glyphs.iter().find(|g| g.chr == c && g.advance_width > 0.0).unwrap().pos.x;
                assert!(x('ج') < x('P') && x('P') < x('ف'), "Arabic name must keep its internal RTL order");
                assert_eq!(galley.job.text, text);
            }
        });
    }

    #[test]
    fn joins_arabic_letters_using_one_font_including_spaces() {
        with_fonts(|ctx| {
            let text = "بِدء مُستند جَديد";
            let galley = plain(ctx, text, FontId::proportional(15.0), Color32::WHITE);
            let expected = ctx.fonts_mut(|fonts| {
                fonts.layout_job(LayoutJob::simple_singleline(text.to_owned(), FontId::new(15.0, FontFamily::Name("arabic".into())), Color32::WHITE))
            });
            assert_eq!(displayed(&galley), displayed(&expected));
            assert_eq!(galley.size(), expected.size());
            // Contextual glyphs (atlas positions) must match the unbroken Arabic shaping run.
            for (actual, expected) in galley.rows[0].glyphs.iter().zip(&expected.rows[0].glyphs) {
                assert_eq!(actual.uv_rect, expected.uv_rect);
            }
        });
    }

    #[test]
    fn wraps_first_logical_word_first_and_right_aligns_lines() {
        with_fonts(|ctx| {
            let text = "مستند جديد للطباعة";
            let width = plain(ctx, "مستند ", FontId::proportional(15.0), Color32::WHITE).size().x + 1.0;
            let mut job = LayoutJob::simple_singleline(text.to_owned(), FontId::proportional(15.0), Color32::WHITE);
            job.wrap = TextWrapping::wrap_at_width(width);
            let galley = layout(ctx, job);
            assert!(galley.rows.len() >= 3);
            let first: String = galley.rows[0].glyphs.iter().map(|g| g.chr).collect();
            assert!(first.contains('م') && first.contains('س') && !first.contains('ج'), "{first}");
            for row in &galley.rows {
                assert!((row.pos.x + row.size.x - galley.size().x).abs() < 0.01);
            }
            assert_eq!(galley.job.text, text);
        });
    }

    #[test]
    fn truncates_long_words_and_handles_empty_lines() {
        with_fonts(|ctx| {
            let mut job = LayoutJob::simple_singleline("مستندجديدللطباعة\n\nPDF 123".to_owned(), FontId::proportional(15.0), Color32::WHITE);
            job.wrap = TextWrapping::truncate_at_width(45.0);
            let galley = layout(ctx, job);
            assert_eq!(galley.rows.len(), 1);
            assert!(galley.elided);
            assert!(galley.size().x <= 45.0);
            assert!(displayed(&galley).contains('…'));
            let mut job = LayoutJob::simple_singleline("ملف\n\nجديد\n".to_owned(), FontId::proportional(15.0), Color32::WHITE);
            job.break_on_newline = true;
            let galley = layout(ctx, job);
            assert_eq!(galley.rows.len(), 4);
            assert!(galley.rect.is_finite());
            let galley = plain(ctx, "ملف\nجديد", FontId::proportional(15.0), Color32::WHITE);
            assert_eq!(galley.rows.len(), 1);
            assert_eq!(galley.job.text, "ملف\nجديد");
        });
    }

    #[test]
    fn ltr_and_rtl_isolates_order_currency_ratios_symbols_and_system_arabic_fallbacks() {
        with_fonts(|ctx| {
            // 1. Dollar currency after Arabic keeps '$' to the left of digits and digits to the left of Arabic.
            let cost_line = format!("التكلفة التقديرية {}", ltr("$0.0398"));
            let galley = plain(ctx, &cost_line, FontId::proportional(15.0), Color32::WHITE);
            assert_eq!(galley.job.text, cost_line);
            let vis = displayed(&galley);
            assert!(!vis.chars().any(is_bidi_isolate_control), "{vis:?}");
            let glyphs = &galley.rows[0].glyphs;
            let x = |c| glyphs.iter().find(|g| g.chr == c && g.advance_width > 0.0).unwrap().pos.x;
            assert!(x('$') < x('0') && x('0') < x('9') && x('9') < x('ة'), "currency order: {vis}");

            // 2. Arabic status line starting with an LTR technical token flows right-to-left under rtl_paragraph.
            let ram_line = rtl_paragraph(format!("{}: {} حرة / إجمالي", ltr("RAM"), ltr("8192 MiB / 16384 MiB")));
            let ram_galley = plain(ctx, &ram_line, FontId::proportional(15.0), Color32::WHITE);
            let ram_glyphs = &ram_galley.rows[0].glyphs;
            let rx = |c| ram_glyphs.iter().find(|g| g.chr == c && g.advance_width > 0.0).unwrap().pos.x;
            assert!(rx('ح') < rx('8') && rx('8') < rx('6') && rx('6') < rx('R'), "RTL paragraph order: {}", displayed(&ram_galley));

            // 3. Leading warning symbol in an RTL label stays on the visual right of the Arabic title.
            let alert_line = "⚠ تنبيه";
            let alert_galley = plain(ctx, alert_line, FontId::proportional(15.0), Color32::WHITE);
            assert_eq!(alert_galley.job.text, alert_line);
            let alert_glyphs = &alert_galley.rows[0].glyphs;
            let ax = |c| alert_glyphs.iter().find(|g| g.chr == c && g.advance_width > 0.0).unwrap().pos.x;
            assert!(ax('ه') < ax('ت') && ax('ت') < ax('⚠'), "leading RTL symbol belongs on visual right: {}", displayed(&alert_galley));
        });

        // 4. Multi-word punctuated Arabic with a system Arab fallback (and empty CRAFT_FONTS) keeps RTL word order.
        let bytes = designcraft_fonts::testing::font_mapping(
            "DC System Arabic",
            &[
                ('ع', 'A'),
                ('م', 'M'),
                ('ل', 'L'),
                ('ف', 'F'),
                ('س', 'S'),
                ('ت', 'T'),
                ('ن', 'N'),
                ('ج', 'J'),
                ('د', 'D'),
                ('ي', 'Y'),
                (' ', ' '),
                (':', ':'),
            ],
        )
        .unwrap();
        let system = [crate::theme::SystemFont::new("Arab", "DC System Arabic".into(), bytes, 0)];
        let mut defs = crate::theme::font_definitions(&[], "ar");
        crate::theme::add_system_fallbacks(&mut defs, &system, "ar");
        let ctx = egui::Context::default();
        ctx.set_fonts(defs);
        ctx.begin_pass(egui::RawInput { max_texture_side: Some(8192), ..Default::default() });
        let galley = plain(&ctx, "ملف: مستند جديد", FontId::proportional(15.0), Color32::WHITE);
        let glyphs = &galley.rows[0].glyphs;
        let gx = |c| glyphs.iter().find(|g| g.chr == c && g.advance_width > 0.0).unwrap().pos.x;
        assert!(gx('ج') < gx('س') && gx('س') < gx('ف'), "multi-word Arabic must stay RTL without CRAFT_FONTS: {}", displayed(&galley));
        ctx.end_pass().textures_delta.clear();
    }

    #[test]
    fn editable_layout_maps_cursors_to_visual_glyphs_combining_marks_and_bidi_clicks() {
        with_fonts(|ctx| {
            let sample = "بِسْمِ اللَّهِ الرَّحْمَنِ\nسطر ثانٍ 123 مع نص عربي طويل للالتفاف التلقائي\n";
            egui::Area::new(egui::Id::new("rtl-editable-test")).show(ctx, |ui| {
                let mut galley = editable_layout(ui, sample, 180.0, FontId::proportional(14.0), Color32::WHITE, true);
                assert_eq!(galley.job.text, sample);
                // Exact char count across soft-wrapped + hard-newline + combining-mark rows.
                assert_eq!(galley.end().index, CharIndex(sample.chars().count()));
                assert!(galley.rows[0].visuals.mesh.vertices.len() >= 4);
                assert!(galley.rect.is_finite());
                assert!(galley.mesh_bounds.is_finite());
                // Soft-wrapped rows must NOT pretend to have hard '\n'; only explicit '\n' rows do.
                let hard_newlines = galley.rows.iter().filter(|r| r.ends_with_newline).count();
                assert_eq!(hard_newlines, 2);

                // First logical char ('ب' at index 0) is displayed on the visual right, NOT at x == 0.
                let first_row = &galley.rows[0];
                let caret_0 = galley.pos_from_cursor(CCursor::new(0)).min.x;
                assert!(
                    (caret_0 - (first_row.pos.x + first_row.size.x)).abs() < 0.5,
                    "logical char 0 belongs at visual right edge: caret_0={caret_0}, right={}",
                    first_row.pos.x + first_row.size.x
                );
                // Combining marks ('ِ' at index 1, 'ْ' at index 3, 'ِ' at index 5) consume zero advance.
                assert_eq!(first_row.glyphs[1].advance_width, 0.0, "kasra must have zero advance");
                assert_eq!(first_row.glyphs[3].advance_width, 0.0, "sukun must have zero advance");
                assert_eq!(first_row.glyphs[5].advance_width, 0.0, "kasra must have zero advance");
                assert!((first_row.glyphs[1].pos.x - first_row.glyphs[2].pos.x).abs() < 1e-3);

                // Mixed BiDi + lam-alef ligature line: "ملف PDF لا جديد"
                let mixed = "ملف PDF لا جديد";
                let mixed_galley = editable_layout(ui, mixed, f32::INFINITY, FontId::proportional(15.0), Color32::WHITE, false);
                assert_eq!(mixed_galley.end().index, CharIndex(mixed.chars().count()));
                let m_row = &mixed_galley.rows[0];
                let center_of = |idx: usize| m_row.pos.x + m_row.glyphs[idx].logical_rect().center().x;
                // Visual order left-to-right: "جديد" (11..15) < "لا" (8..10) < "PDF" (4..7) < "ملف" (0..3).
                assert!(center_of(14) < center_of(11), "Arabic 'جديد' flows RTL on the left");
                assert!(center_of(11) < center_of(9) && center_of(9) < center_of(8), "lam-alef ligature splits RTL ('ا' left of 'ل')");
                assert!(center_of(8) < center_of(4) && center_of(4) < center_of(6), "Latin 'PDF' flows LTR in the middle");
                assert!(center_of(6) < center_of(2) && center_of(2) < center_of(0), "Arabic 'ملف' flows RTL on the right");

                // Clicking on displayed characters targets the exact displayed logical index/word.
                let y = m_row.rect().center().y;
                let click_meem = bidi_cursor_from_pos(&mixed_galley, egui::vec2(center_of(0) + 2.0, y));
                assert_eq!(click_meem.index, CharIndex(0));
                let (_, word_jadid) = bidi_hit_pos(&mixed_galley, egui::vec2(center_of(12), y));
                let selected_jadid = select_word_range(mixed, word_jadid);
                assert_eq!(selected_jadid.slice_str(mixed), "جديد");
                let (_, word_pdf) = bidi_hit_pos(&mixed_galley, egui::vec2(center_of(5), y));
                assert_eq!(select_word_range(mixed, word_pdf).slice_str(mixed), "PDF");
                let (_, word_malaf) = bidi_hit_pos(&mixed_galley, egui::vec2(center_of(1), y));
                assert_eq!(select_word_range(mixed, word_malaf).slice_str(mixed), "ملف");

                // Arbitrary and full-range selection highlights never panic or corrupt vertices.
                for (a, b) in [(2, sample.chars().count().saturating_sub(3)), (0, sample.chars().count())] {
                    let range = CCursorRange::two(CCursor::new(a), CCursor::new(b));
                    egui::text_selection::visuals::paint_text_selection(&mut galley, ui.visuals(), &range, None);
                }
            });
        });

        // End-to-end TextEdit test: clicking the leftmost displayed word ("جديد") in "ملف جديد"
        // and typing inserts at "جديد", NOT at logical index 0 ("ملف").
        let ctx = egui::Context::default();
        ctx.set_fonts(crate::theme::font_definitions(designcraft_fonts::CRAFT_FONTS, "ar"));
        let mut buf = "ملف جديد".to_owned();
        let mut jadid_left_click_pos = Pos2::ZERO;
        let screen = Some(Rect::from_min_size(Pos2::ZERO, egui::vec2(600.0, 400.0)));
        ctx.run_ui(egui::RawInput { screen_rect: screen, max_texture_side: Some(8192), ..Default::default() }, |ui| {
            let mut layouter = text_edit_layouter(false, Some(FontId::proportional(16.0)), None);
            let out = show_text_edit(
                ui,
                egui::TextEdit::singleline(&mut buf)
                    .id(egui::Id::new("rtl-click-field"))
                    .horizontal_align(Align::Max)
                    .desired_width(240.0)
                    .layouter(&mut layouter),
            );
            let painted_origin = out.galley_pos - egui::vec2(out.galley.rect.left(), 0.0);
            let last_char_left = painted_origin.x + out.galley.rows[0].pos.x + out.galley.rows[0].glyphs[7].max_x();
            let mid_y = painted_origin.y + out.galley.rows[0].rect().center().y;
            jadid_left_click_pos = Pos2::new(last_char_left + 0.5, mid_y);
        })
        .textures_delta
        .clear();

        // Frame 2: click at the visual left edge (after 'د' of "جديد", logical index 8).
        let mut clicked_cursor = None;
        ctx.run_ui(
            egui::RawInput {
                screen_rect: screen,
                max_texture_side: Some(8192),
                focused: true,
                events: vec![
                    egui::Event::PointerMoved(jadid_left_click_pos),
                    egui::Event::PointerButton {
                        pos: jadid_left_click_pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ui| {
                let mut layouter = text_edit_layouter(false, Some(FontId::proportional(16.0)), None);
                let out = show_text_edit(
                    ui,
                    egui::TextEdit::singleline(&mut buf)
                        .id(egui::Id::new("rtl-click-field"))
                        .horizontal_align(Align::Max)
                        .desired_width(240.0)
                        .layouter(&mut layouter),
                );
                clicked_cursor = out.cursor_range.map(|r| r.primary.index);
            },
        )
        .textures_delta
        .clear();
        assert_eq!(clicked_cursor, Some(CharIndex(8)));

        // Frame 3: release mouse button and type "!" — must append at index 8 ("ملف جديد!"), not index 0.
        ctx.run_ui(
            egui::RawInput {
                screen_rect: screen,
                max_texture_side: Some(8192),
                focused: true,
                events: vec![
                    egui::Event::PointerButton {
                        pos: jadid_left_click_pos,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    },
                    egui::Event::Text("!".into()),
                ],
                ..Default::default()
            },
            |ui| {
                let mut layouter = text_edit_layouter(false, Some(FontId::proportional(16.0)), None);
                show_text_edit(
                    ui,
                    egui::TextEdit::singleline(&mut buf)
                        .id(egui::Id::new("rtl-click-field"))
                        .horizontal_align(Align::Max)
                        .desired_width(240.0)
                        .layouter(&mut layouter),
                );
            },
        )
        .textures_delta
        .clear();
        assert_eq!(buf, "ملف جديد!");
    }
}
