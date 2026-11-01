//! Interface text bidi & Arabic shaping: read-only galleys (`widget`, `label`, `plain`) and
//! visual-cursor-mapped editable galleys (`editable_layout`, `show_text_edit`) for `egui::TextEdit`.
//! Read-only source strings stay logical: epaint resolves each paragraph's direction
//! (`LayoutJob::direction == Auto`), runs the Unicode bidi algorithm once and orders a row's
//! glyphs for painting while the glyph array and `Galley::job.text` stay logical.
use egui::epaint::text::TextDirection;
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align, Align2, Color32, FontId, Galley, Pos2, Rect, WidgetText};
use std::{collections::HashMap, sync::Arc};
use unicode_bidi::{BidiClass, BidiInfo, bidi_class};

/// Allocation-free preflight: ordinary Latin, CJK and emoji labels need no cache lookup.
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
    if text.is_ascii() {
        return false;
    }
    let mut strong_rtl = false;
    let mut needs_uba = false;
    let mut has_lro = false;
    for c in text.chars() {
        match bidi_class(c) {
            BidiClass::R | BidiClass::AL => strong_rtl = true,
            BidiClass::LRO => {
                has_lro = true;
                needs_uba = true;
            }
            BidiClass::AN
            | BidiClass::LRE
            | BidiClass::RLE
            | BidiClass::RLO
            | BidiClass::PDF
            | BidiClass::LRI
            | BidiClass::RLI
            | BidiClass::FSI
            | BidiClass::PDI => needs_uba = true,
            _ => {}
        }
    }
    if strong_rtl && !has_lro {
        return true;
    }
    (strong_rtl || needs_uba) && BidiInfo::new(text, None).has_rtl()
}

/// Resolve translated labels through the shaper, without changing IDs, commands or stored text.
fn widget_rtl_known(ui: &egui::Ui, text: WidgetText) -> WidgetText {
    let mut job = (*text.into_layout_job(ui.style(), egui::FontSelection::Default, Align::Center)).clone();
    job.direction = TextDirection::Auto;
    if job.language.is_none() {
        job.language = ui.language();
    }
    job.wrap = TextWrapping::from_wrap_mode_and_width(ui.wrap_mode(), ui.available_width());
    WidgetText::Galley(layout(ui.ctx(), job))
}

/// Resolve translated labels through the shaper, without changing IDs, commands or stored text.
pub fn widget(ui: &egui::Ui, text: impl Into<WidgetText>) -> WidgetText {
    let text = text.into();
    if !may_have_rtl(text.text()) {
        return text;
    }
    let mut job = (*text.into_layout_job(ui.style(), egui::FontSelection::Default, Align::Center)).clone();
    // The shaper owns bidi: it keeps the glyphs in logical order and gives each row a visual
    // order (and the paragraph's start edge), so nothing here may reorder or mirror text.
    job.direction = TextDirection::Auto;
    if job.language.is_none() {
        job.language = ui.language();
    }
    job.wrap = TextWrapping::from_wrap_mode_and_width(ui.wrap_mode(), ui.available_width());
    WidgetText::Galley(layout(ui.ctx(), job))
}

/// Resolve window titles or headings where only `egui::Context` is available.
pub fn widget_ctx(ctx: &egui::Context, text: impl Into<WidgetText>) -> WidgetText {
    let text = text.into();
    if !may_have_rtl(text.text()) {
        return text;
    }
    let style = ctx.global_style();
    let mut job = (*text.into_layout_job(&style, egui::FontSelection::Style(egui::TextStyle::Heading), Align::Center)).clone();
    job.direction = TextDirection::Auto;
    WidgetText::Galley(layout(ctx, job))
}

/// Labels are read-only: no editable text/caret mapping is derived from a shaped galley.
/// Accessibility still receives the original logical string through `Galley::job`.
pub fn label(ui: &mut egui::Ui, text: impl Into<WidgetText>) -> egui::Response {
    let text = text.into();
    let rtl = may_have_rtl(text.text());
    if !rtl {
        return ui.label(text);
    }
    let selectable = false;
    let rendered = widget_rtl_known(ui, text.clone());
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

const MAX_CACHED_LABELS: usize = 2048;
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

/// Lay out text without RTL candidates directly; otherwise use a bounded job cache that checks full job equality and invalidates on the font token.
pub(crate) fn layout(ctx: &egui::Context, job: LayoutJob) -> Arc<Galley> {
    if !may_have_rtl(&job.text) {
        return ctx.fonts_mut(|fonts| fonts.layout_job(job));
    }
    // Keep unusually large/dynamic content from filling a UI-label cache.
    if job.text.len() > MAX_CACHED_TEXT_BYTES || job.sections.len() > 256 {
        return layout_uncached(ctx, job);
    }
    let token = ctx.fonts_mut(|fonts| fonts.layout_no_wrap(String::new(), FontId::default(), Color32::TRANSPARENT));
    let id = egui::Id::unique("designcraft.rtl.layout-cache");
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

/// Set paragraph direction to `Auto` and preserve the logical source, including line separators in a single-line label.
fn layout_uncached(ctx: &egui::Context, mut job: LayoutJob) -> Arc<Galley> {
    job.direction = TextDirection::Auto;
    // The 0.37 shaper owns single-line handling as well as bidi. Preserve the logical source
    // for accessibility and cache equality, including line separators in a single-line label.
    ctx.fonts_mut(|fonts| fonts.layout_job(job))
}

/// Lay out text for an editable [`egui::TextEdit`] using `epaint`'s native bidi shaper (`TextDirection::Auto`).
pub fn editable_layout(ui: &egui::Ui, text: &str, wrap_width: f32, font_id: FontId, text_color: Color32, multiline: bool) -> Arc<Galley> {
    let mut job = if multiline {
        LayoutJob::simple(text.to_owned(), font_id.clone(), text_color, wrap_width)
    } else {
        LayoutJob::simple_singleline(text.to_owned(), font_id.clone(), text_color)
    };
    job.direction = TextDirection::Auto;
    job.halign = ui.layout().horizontal_align();
    job.language = ui.language();
    job.keep_trailing_whitespace = true;
    let line_height = ui.fonts_mut(|f| f.row_height(&font_id)) + ui.spacing().extra_text_line_spacing;
    for section in &mut job.sections {
        section.format.line_height = Some(line_height);
    }
    ui.fonts_mut(|fonts| fonts.layout_job(job))
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

/// Show a [`egui::TextEdit`] with native `epaint` bidi shaping and cursor interaction.
pub fn show_text_edit(ui: &mut egui::Ui, edit: egui::TextEdit<'_>) -> egui::text_edit::TextEditOutput {
    let mut out = edit.show(ui);
    if out.cursor_range.is_none() {
        out.cursor_range = out.state.cursor.char_range();
    }
    out
}

/// Place a [`egui::TextEdit`] inside `max_rect` (like [`egui::Ui::put`]) with native bidi interaction.
pub fn put_text_edit(ui: &mut egui::Ui, max_rect: Rect, edit: egui::TextEdit<'_>) -> egui::Response {
    ui.scope_builder(egui::UiBuilder::new().max_rect(max_rect), |ui| show_text_edit(ui, edit)).inner.response.response
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::FontFamily;
    use egui::text::{CCursor, CCursorRange, CharIndex};

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

    /// The glyphs in the order they are painted, left to right per row.
    fn drawn(galley: &Galley) -> String {
        let mut out = String::new();
        for row in &galley.rows {
            for i in row.row.visual_order().filter(|&i| row.row.glyphs.get(i).is_some_and(|g| g.advance_width > 0.0)) {
                if let Some(glyph) = row.row.glyphs.get(i) {
                    out.push(glyph.chr);
                }
            }
        }
        out
    }

    /// The glyphs in logical order (the stored order, independent of the paragraph direction).
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

    /// Latin, CJK, combining and emoji labels skip the RTL path; Arabic/Hebrew, controls and Arabic-Indic digits take it.
    #[test]
    fn ordinary_multilingual_labels_skip_the_rtl_layout_cache() {
        for text in ["", "File (123)", "Édition", "中文排版", "スウォッチ", "e\u{301}", "🎨 🙂"] {
            assert!(!may_have_rtl(text), "{text:?}");
        }
        for text in ["ملف", "עברית", "PDF ملف 123", "\u{1e900}", "\u{200f}", "\u{202e}abc\u{202c}", "\u{2067}abc\u{2069}", "١٢٣"] {
            assert!(may_have_rtl(text), "{text:?}");
        }
    }

    /// Non-RTL widgets remain `RichText`, keep their text and lay out with finite bounds.
    #[test]
    fn non_rtl_widgets_keep_native_text_and_formatting() {
        with_fonts(|ctx| {
            egui::Area::new(egui::Id::unique("rtl-fast-path-test")).show(ctx, |ui| {
                for text in ["中文", "日本語", "Édition", "🎨"] {
                    let rendered = widget(ui, egui::RichText::new(text).strong());
                    assert!(matches!(rendered, WidgetText::RichText(_)));
                    assert_eq!(rendered.text(), text);
                    assert!(label(ui, text).rect.is_finite());
                }
            });
        });
    }

    /// Source and glyph arrays stay logical; painting applies bidi, mirrors brackets, keeps `PDF 123` left-to-right and gives finite metrics.
    #[test]
    fn mixed_arabic_latin_digits_and_brackets_preserve_logical_source() {
        with_fonts(|ctx| {
            let text = "ملف (PDF 123) جديد";
            let galley = plain(ctx, text, FontId::proportional(15.0), Color32::WHITE);
            // The source stays logical in both the job and the glyph array.
            assert_eq!(galley.job.text, text);
            let row = &galley.rows[0].row;
            assert_eq!(row.text(), text);
            // Painting is reordered once by the shaper: the last logical word is drawn leftmost,
            // and the Latin number inside the Arabic sentence still reads left to right.
            let visual = drawn(&galley);
            assert!(visual.contains("PDF 123"), "{visual}");
            assert!(visual.starts_with('د'), "Arabic final word belongs at the left: {visual}");
            let x = |c: char| row.glyphs.iter().find(|g| g.chr == c && g.advance_width > 0.0).unwrap().pos.x;
            // The shaper mirrors the brackets: the logical ')' is drawn as '(' left of the number,
            // the logical '(' as ')' right of it.
            assert!(x(')') < x('P') && x('3') < x('('), "brackets around the number: {visual}");
            // Letters inside the right-to-left words are placed right to left.
            assert!(x('ج') < x('ف'), "Arabic word order: {visual}");
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

    /// Fallback shaping matches the native Auto-direction shaper in displayed glyphs, sizes, atlas UVs, advances and spaces.
    #[test]
    fn arabic_fallback_matches_the_native_shaper_including_spaces() {
        with_fonts(|ctx| {
            let text = "بِدء مُستند جَديد";
            let galley = plain(ctx, text, FontId::proportional(15.0), Color32::WHITE);
            let expected = ctx.fonts_mut(|fonts| {
                // The native shaper resolves Arabic clusters from the requested UI family. A
                // different Arabic-primary family changes spaces and line metrics too; it is
                // not an oracle for fallback shaping in a proportional UI label.
                let mut job = LayoutJob::simple_singleline(text.to_owned(), FontId::proportional(15.0), Color32::WHITE);
                job.direction = TextDirection::Auto;
                fonts.layout_job(job)
            });
            assert_eq!(galley.job.text, text);
            assert_eq!(displayed(&galley), displayed(&expected));
            assert_eq!(galley.size(), expected.size());
            assert_eq!(galley.rows[0].glyphs.len(), expected.rows[0].glyphs.len());
            // Contextual glyphs (atlas positions) must match the native logical shaping run.
            for (actual, expected) in galley.rows[0].glyphs.iter().zip(&expected.rows[0].glyphs) {
                assert_eq!(actual.uv_rect, expected.uv_rect);
                assert_eq!(actual.advance_width, expected.advance_width);
            }
        });
    }

    /// Logical wrapping puts the first word first and every RTL row flush right within one pixel.
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
            // Every right-to-left row is flush with the right edge. Rows are pixel-snapped, so the
            // shared edge can differ from the galley width by less than one pixel.
            for row in &galley.rows {
                assert!((row.pos.x + row.size.x - galley.size().x).abs() < 1.0, "{:?}", row.pos);
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
            assert!(!vis.chars().any(|c| matches!(c, '\u{2066}'..='\u{2069}')), "{vis:?}");
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
            egui::Area::new(egui::Id::unique("rtl-editable-test")).show(ctx, |ui| {
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
                assert!(first_row.glyphs[0].is_rtl(), "Arabic base glyph carries RTL bidi level");

                // Mixed BiDi + lam-alef ligature line: "ملف PDF لا جديد"
                let mixed = "ملف PDF لا جديد";
                let mixed_galley = editable_layout(ui, mixed, f32::INFINITY, FontId::proportional(15.0), Color32::WHITE, false);
                assert_eq!(mixed_galley.end().index, CharIndex(mixed.chars().count()));
                let m_row = &mixed_galley.rows[0];
                let center_of = |idx: usize| m_row.pos.x + m_row.glyphs[idx].logical_rect().center().x;
                // Visual order left-to-right: "جديد" (11..15) < "لا" (8..10) < "PDF" (4..7) < "ملف" (0..3).
                assert!(center_of(14) < center_of(11), "Arabic 'جديد' flows RTL on the left");
                assert!(center_of(11) <= center_of(9) && center_of(9) <= center_of(8), "lam-alef ligature stays between 'جديد' and 'PDF'");
                assert!(center_of(8) < center_of(4) && center_of(4) < center_of(6), "Latin 'PDF' flows LTR in the middle");
                assert!(center_of(6) < center_of(2) && center_of(2) < center_of(0), "Arabic 'ملف' flows RTL on the right");

                // Clicking on displayed characters targets the exact displayed logical index.
                let y = m_row.rect().center().y;
                let click_meem = mixed_galley.cursor_from_pos(egui::vec2(m_row.pos.x + m_row.glyphs[0].max_x() - 0.5, y));
                assert_eq!(click_meem.index, CharIndex(0));
                let click_jadid = mixed_galley.cursor_from_pos(egui::vec2(center_of(12), y));
                assert!((11..=15).contains(&click_jadid.index.0), "clicked inside 'جديد': {:?}", click_jadid);
                let click_pdf = mixed_galley.cursor_from_pos(egui::vec2(center_of(5), y));
                assert!((4..=7).contains(&click_pdf.index.0), "clicked inside 'PDF': {:?}", click_pdf);
                let click_malaf = mixed_galley.cursor_from_pos(egui::vec2(center_of(1), y));
                assert!((0..=3).contains(&click_malaf.index.0), "clicked inside 'ملف': {:?}", click_malaf);

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
                    .id(egui::Id::unique("rtl-click-field"))
                    .align(Align2::RIGHT_CENTER)
                    .desired_width(240.0)
                    .layouter(&mut layouter),
            );
            let painted_origin = out.galley_pos - egui::vec2(out.galley.rect.left(), 0.0);
            let last_char_left = painted_origin.x + out.galley.rows[0].pos.x + out.galley.rows[0].glyphs[7].pos.x;
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
                        .id(egui::Id::unique("rtl-click-field"))
                        .align(Align2::RIGHT_CENTER)
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
                        .id(egui::Id::unique("rtl-click-field"))
                        .align(Align2::RIGHT_CENTER)
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
