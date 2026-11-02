//! Coordinate-aware bilingual (LTR + Arabic RTL) PDF text extraction via `hayro-interpret`.

use hayro_cmap::BfString;
use hayro_interpret::font::Glyph;
use hayro_interpret::{
    BlendMode, ClipPath, Context, Device, GlyphDrawMode, Image, InterpreterCache, InterpreterSettings, Paint, PathDrawMode, SoftMask, interpret_page,
};
use hayro_syntax::Pdf;
use kurbo::{Affine, BezPath, Point, Rect, Shape as _};

/// Maximum input PDF byte size accepted for text extraction (128 MiB).
pub const MAX_EXTRACT_PDF_BYTES: usize = 128 * 1024 * 1024;

/// Maximum total glyphs recorded across a single PDF document to bound memory against PDF bombs.
pub const MAX_EXTRACT_GLYPHS: usize = 2_000_000;

/// Extracted text and layout statistics for a single PDF page.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtractedPdfPage {
    /// 1-based page number.
    pub page_number: usize,
    pub width_pt: f64,
    pub height_pt: f64,
    pub text: String,
    pub glyph_count: usize,
    pub image_count: usize,
    /// True when the page draws raster images (`image_count > 0`) but has `< 16` extracted non-whitespace characters.
    pub is_likely_scanned: bool,
}

/// Complete text extraction report across all pages of a PDF document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PdfExtractionReport {
    pub page_count: usize,
    pub pages: Vec<ExtractedPdfPage>,
    pub combined_text: String,
    pub has_scanned_pages: bool,
}

#[derive(Clone, Debug)]
struct RawGlyph {
    text: String,
    origin: Point,
    ink_box: Option<Rect>,
    est_size: f64,
}

struct PageExtractor {
    glyphs: Vec<RawGlyph>,
    glyph_count: usize,
    image_count: usize,
    remaining_budget: usize,
    budget_exceeded: bool,
}

impl Device<'_> for PageExtractor {
    fn set_soft_mask(&mut self, _: Option<SoftMask<'_>>) {}
    fn set_blend_mode(&mut self, _: BlendMode) {}
    fn draw_path(&mut self, _: &BezPath, _: Affine, _: &Paint<'_>, _: &PathDrawMode) {}
    fn push_clip_path(&mut self, _: &ClipPath) {}
    fn push_transparency_group(&mut self, _: f32, _: Option<SoftMask<'_>>, _: BlendMode) {}

    fn draw_glyph(&mut self, g: &Glyph<'_>, xf: Affine, gxf: Affine, _: &Paint<'_>, _: &GlyphDrawMode) {
        if self.budget_exceeded {
            return;
        }
        if self.remaining_budget == 0 {
            self.budget_exceeded = true;
            return;
        }
        self.remaining_budget -= 1;
        self.glyph_count += 1;

        let text = match g.as_unicode() {
            Some(BfString::Char(c)) => c.to_string(),
            Some(BfString::String(s)) => s,
            None => return,
        };
        if text.is_empty() {
            return;
        }

        let full = xf * gxf;
        let origin = full * Point::ZERO;
        if !origin.x.is_finite() || !origin.y.is_finite() {
            return;
        }

        let (ink_box, est_size) = glyph_geometry(g, full);
        self.glyphs.push(RawGlyph { text, origin, ink_box, est_size });
    }

    fn draw_image(&mut self, _: Image<'_, '_>, _: Affine) {
        self.image_count = self.image_count.saturating_add(1);
    }

    fn pop_clip_path(&mut self) {}
    fn pop_transparency_group(&mut self) {}
}

fn glyph_geometry(g: &Glyph<'_>, full: Affine) -> (Option<Rect>, f64) {
    if let Glyph::Outline(outline) = g {
        let mut path = outline.outline();
        path.apply_affine(full);
        let bb = path.bounding_box();
        let valid = bb.x0.is_finite() && bb.y0.is_finite() && bb.x1.is_finite() && bb.y1.is_finite() && (bb.width() > 0.0 || bb.height() > 0.0);
        if valid {
            let est = (bb.height() * 1.35).max(bb.width()).clamp(4.0, 288.0);
            return (Some(bb), est);
        }
    }
    let c = full.as_coeffs();
    let sy = c[2].hypot(c[3]);
    let sx = c[0].hypot(c[1]);
    let raw = sy.max(sx);
    // Outline fonts in hayro typically scale by font_size / upem (often 1000 or 2048).
    let est = if raw > 0.0 && raw < 0.5 {
        (raw * 1000.0).clamp(4.0, 288.0)
    } else if raw.is_finite() && raw >= 0.5 {
        raw.clamp(4.0, 288.0)
    } else {
        12.0
    };
    (None, est)
}

/// Extract coordinate-reconstructed, Arabic-normalized text for every page of `bytes`.
pub fn extract_text_pages(bytes: &[u8]) -> std::result::Result<PdfExtractionReport, String> {
    if bytes.is_empty() {
        return Err("PDF input is empty".into());
    }
    if bytes.len() > MAX_EXTRACT_PDF_BYTES {
        return Err(format!("PDF exceeds maximum size of {} MiB", MAX_EXTRACT_PDF_BYTES / (1024 * 1024)));
    }

    let pdf = Pdf::new(bytes.to_vec()).map_err(|e| format!("invalid or unsupported PDF: {e:?}"))?;
    let cache = InterpreterCache::new();
    let pdf_pages = pdf.pages();
    let page_count = pdf_pages.len();
    let mut pages = Vec::with_capacity(page_count);
    let mut remaining_budget = MAX_EXTRACT_GLYPHS;

    for (idx, page) in pdf_pages.iter().enumerate() {
        let (rw, rh) = page.render_dimensions();
        let width_pt = if rw.is_finite() && rw > 0.0 { f64::from(rw) } else { 0.0 };
        let height_pt = if rh.is_finite() && rh > 0.0 { f64::from(rh) } else { 0.0 };

        let clip = kurbo::Rect::new(0.0, 0.0, width_pt.max(1.0), height_pt.max(1.0));
        let mut ctx = Context::new(Affine::IDENTITY, clip, &cache, pdf.xref(), InterpreterSettings::default());
        let mut dev = PageExtractor { glyphs: Vec::new(), glyph_count: 0, image_count: 0, remaining_budget, budget_exceeded: false };
        interpret_page(page, &mut ctx, &mut dev);
        if dev.budget_exceeded {
            return Err(format!("PDF exceeds maximum extracted glyph limit ({MAX_EXTRACT_GLYPHS})"));
        }
        remaining_budget = dev.remaining_budget;

        let text = reconstruct_page_text(&dev.glyphs);
        let non_ws_chars = text.chars().filter(|c| !c.is_whitespace() && *c != '\u{FFFD}').count();
        let is_likely_scanned = dev.image_count > 0 && non_ws_chars < 16;

        pages.push(ExtractedPdfPage {
            page_number: idx + 1,
            width_pt,
            height_pt,
            text,
            glyph_count: dev.glyph_count,
            image_count: dev.image_count,
            is_likely_scanned,
        });
    }

    let has_scanned_pages = pages.iter().any(|p| p.is_likely_scanned);
    let mut combined_text = String::new();
    for page in &pages {
        if page.text.trim().is_empty() {
            continue;
        }
        if !combined_text.is_empty() {
            combined_text.push_str(&format!("\n\n--- Page {} ---\n", page.page_number));
        } else if page_count > 1 {
            combined_text.push_str(&format!("--- Page {} ---\n", page.page_number));
        }
        combined_text.push_str(&page.text);
    }

    Ok(PdfExtractionReport { page_count, pages, combined_text, has_scanned_pages })
}

#[derive(Clone, Debug)]
struct LineToken {
    text: String,
    origin_x: f64,
    is_space: bool,
    has_pres_arabic: bool,
}

fn is_combining_or_mark(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            matches!(
                c as u32,
                0x0300..=0x036F
                    | 0x0483..=0x0489
                    | 0x0591..=0x05BD
                    | 0x05BF
                    | 0x05C1..=0x05C2
                    | 0x05C4..=0x05C5
                    | 0x05C7
                    | 0x0610..=0x061A
                    | 0x064B..=0x065F
                    | 0x0670
                    | 0x06D6..=0x06ED
                    | 0xFE70..=0xFE7F
            )
        })
}

fn is_arabic_presentation_char(c: char) -> bool {
    matches!(c as u32, 0xFB50..=0xFDFF | 0xFE70..=0xFEFC)
}

fn reconstruct_page_text(glyphs: &[RawGlyph]) -> String {
    if glyphs.is_empty() {
        return String::new();
    }

    // Group glyphs into visual lines whenever baseline `y` moves beyond threshold.
    let mut lines: Vec<Vec<&RawGlyph>> = Vec::new();
    let mut line_ref_y = 0.0_f64;
    let mut line_est_size = 12.0_f64;

    for g in glyphs {
        let is_ws = g.text.chars().all(char::is_whitespace);
        let is_mark = is_combining_or_mark(&g.text);
        if lines.is_empty() {
            line_ref_y = g.origin.y;
            if !is_ws && !is_mark {
                line_est_size = g.est_size;
            }
            lines.push(vec![g]);
            continue;
        }

        if is_mark {
            if let Some(last_line) = lines.last_mut() {
                last_line.push(g);
            }
            continue;
        }

        let effective_size = if is_ws { line_est_size } else { line_est_size.max(g.est_size) };
        let y_tol = (effective_size * 0.35).max(2.5);
        if (g.origin.y - line_ref_y).abs() > y_tol {
            line_ref_y = g.origin.y;
            if !is_ws {
                line_est_size = g.est_size;
            }
            lines.push(vec![g]);
        } else {
            if !is_ws {
                // Smoothly update baseline reference and line em estimate.
                line_ref_y = line_ref_y * 0.75 + g.origin.y * 0.25;
                line_est_size = line_est_size.max(g.est_size);
            }
            if let Some(last_line) = lines.last_mut() {
                last_line.push(g);
            }
        }
    }

    let mut rendered_lines = Vec::with_capacity(lines.len());
    for line in lines {
        let s = reconstruct_line(&line);
        let trimmed = s.trim_end();
        if !trimmed.is_empty() {
            rendered_lines.push(trimmed.to_string());
        }
    }
    rendered_lines.join("\n")
}

fn reconstruct_line(line: &[&RawGlyph]) -> String {
    let line_em = line
        .iter()
        .filter(|g| !g.text.chars().all(char::is_whitespace) && !is_combining_or_mark(&g.text))
        .map(|g| g.est_size)
        .fold(0.0_f64, f64::max)
        .clamp(6.0, 288.0);
    let space_gap_threshold = (line_em * 0.28).max(2.2);

    let mut tokens: Vec<LineToken> = Vec::with_capacity(line.len());
    let mut prev_ink: Option<Rect> = None;
    let mut prev_origin_x: Option<f64> = None;

    for g in line {
        if g.text == "\n" || g.text == "\r" {
            continue;
        }
        if g.text.chars().all(char::is_whitespace) {
            if tokens.last().is_some_and(|t| !t.is_space) {
                tokens.push(LineToken { text: " ".into(), origin_x: g.origin.x, is_space: true, has_pres_arabic: false });
            }
            prev_ink = None;
            prev_origin_x = None;
            continue;
        }

        let is_mark = is_combining_or_mark(&g.text);
        if !is_mark && tokens.last().is_some_and(|t| !t.is_space) {
            let need_space = match (prev_ink, g.ink_box) {
                (Some(a), Some(b)) => {
                    let gap = (b.x0 - a.x1).max(a.x0 - b.x1);
                    gap > space_gap_threshold
                }
                _ => prev_origin_x.is_some_and(|px| (g.origin.x - px).abs() > line_em * 0.9),
            };
            if need_space {
                let mid_x = prev_origin_x.map_or(g.origin.x, |px| (px + g.origin.x) * 0.5);
                tokens.push(LineToken { text: " ".into(), origin_x: mid_x, is_space: true, has_pres_arabic: false });
            }
        }

        let has_pres = g.text.chars().any(is_arabic_presentation_char);
        if is_mark
            && let Some(prev) = tokens.last_mut()
            && !prev.is_space
        {
            prev.text.push_str(&g.text);
            prev.has_pres_arabic |= has_pres;
            continue;
        }

        tokens.push(LineToken { text: g.text.clone(), origin_x: g.origin.x, is_space: false, has_pres_arabic: has_pres });
        if !is_mark {
            prev_ink = g.ink_box;
            prev_origin_x = Some(g.origin.x);
        }
    }

    reverse_visual_ltr_presentation_runs(&mut tokens);

    let mut raw = String::new();
    for tok in &tokens {
        raw.push_str(&tok.text);
    }
    normalize_arabic_presentation_forms(&raw)
}

/// Detect contiguous runs of Arabic Presentation Form tokens that were emitted left-to-right
/// (strictly/predominantly increasing `origin_x`) and reverse those runs into logical RTL order
/// *before* expanding multi-character ligatures like `ﻻ` (`\u{FEFB}` -> `"لا"`).
fn reverse_visual_ltr_presentation_runs(tokens: &mut [LineToken]) {
    let len = tokens.len();
    let mut i = 0;
    while i < len {
        if !tokens[i].has_pres_arabic {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 1;
        while end < len {
            if tokens[end].has_pres_arabic {
                end += 1;
            } else if tokens[end].is_space && tokens.get(end + 1).is_some_and(|next| next.has_pres_arabic) {
                end += 2;
            } else {
                break;
            }
        }

        let mut inc = 0usize;
        let mut dec = 0usize;
        let mut prev_x: Option<f64> = None;
        for tok in &tokens[start..end] {
            if tok.is_space {
                continue;
            }
            if let Some(px) = prev_x {
                let dx = tok.origin_x - px;
                if dx > 0.05 {
                    inc += 1;
                } else if dx < -0.05 {
                    dec += 1;
                }
            }
            prev_x = Some(tok.origin_x);
        }

        if inc > dec {
            tokens[start..end].reverse();
            for tok in &mut tokens[start..end] {
                if tok.has_pres_arabic && tok.text.chars().count() > 1 {
                    let chars: Vec<char> = tok.text.chars().collect();
                    // Keep combining marks attached after their base character while reversing base order.
                    tok.text = reverse_grapheme_like_clusters(&chars);
                }
            }
        }

        i = end;
    }
}

fn reverse_grapheme_like_clusters(chars: &[char]) -> String {
    let mut clusters: Vec<String> = Vec::new();
    for &c in chars {
        let mut buf = [0u8; 4];
        let s = c.encode_utf8(&mut buf);
        if is_combining_or_mark(s)
            && let Some(last) = clusters.last_mut()
        {
            last.push(c);
            continue;
        }
        clusters.push(s.to_string());
    }
    clusters.reverse();
    clusters.concat()
}

/// Normalize Arabic Presentation Forms-A (`U+FB50..=U+FDFF`) and Presentation Forms-B
/// (`U+FE70..=U+FEFF`, including Lam-Alef ligatures `ﻻ`/`ﻷ`/`ﻹ`/`ﻵ`) into standard canonical
/// Arabic (`U+0600..=U+06FF`).
pub fn normalize_arabic_presentation_forms(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match map_arabic_presentation_char(ch) {
            Some(mapped) => out.push_str(mapped),
            None => out.push(ch),
        }
    }
    out
}

fn map_arabic_presentation_char(ch: char) -> Option<&'static str> {
    match ch as u32 {
        // Arabic Presentation Forms-B: Tashkeel (U+FE70..=U+FE7F)
        0xFE70 | 0xFE71 => Some("\u{064B}"),
        0xFE72 => Some("\u{064C}"),
        0xFE73 => Some(""),
        0xFE74 => Some("\u{064D}"),
        0xFE76 | 0xFE77 => Some("\u{064E}"),
        0xFE78 | 0xFE79 => Some("\u{064F}"),
        0xFE7A | 0xFE7B => Some("\u{0650}"),
        0xFE7C | 0xFE7D => Some("\u{0651}"),
        0xFE7E | 0xFE7F => Some("\u{0652}"),
        // Arabic Presentation Forms-B: Contextual letters (U+FE80..=U+FEF4)
        0xFE80 => Some("\u{0621}"),          // ء
        0xFE81..=0xFE82 => Some("\u{0622}"), // آ
        0xFE83..=0xFE84 => Some("\u{0623}"), // أ
        0xFE85..=0xFE86 => Some("\u{0624}"), // ؤ
        0xFE87..=0xFE88 => Some("\u{0625}"), // إ
        0xFE89..=0xFE8C => Some("\u{0626}"), // ئ
        0xFE8D..=0xFE8E => Some("\u{0627}"), // ا
        0xFE8F..=0xFE92 => Some("\u{0628}"), // ب
        0xFE93..=0xFE94 => Some("\u{0629}"), // ة
        0xFE95..=0xFE98 => Some("\u{062A}"), // ت
        0xFE99..=0xFE9C => Some("\u{062B}"), // ث
        0xFE9D..=0xFEA0 => Some("\u{062C}"), // ج
        0xFEA1..=0xFEA4 => Some("\u{062D}"), // ح
        0xFEA5..=0xFEA8 => Some("\u{062E}"), // خ
        0xFEA9..=0xFEAA => Some("\u{062F}"), // د
        0xFEAB..=0xFEAC => Some("\u{0630}"), // ذ
        0xFEAD..=0xFEAE => Some("\u{0631}"), // ر
        0xFEAF..=0xFEB0 => Some("\u{0632}"), // ز
        0xFEB1..=0xFEB4 => Some("\u{0633}"), // س
        0xFEB5..=0xFEB8 => Some("\u{0634}"), // ش
        0xFEB9..=0xFEBC => Some("\u{0635}"), // ص
        0xFEBD..=0xFEC0 => Some("\u{0636}"), // ض
        0xFEC1..=0xFEC4 => Some("\u{0637}"), // ط
        0xFEC5..=0xFEC8 => Some("\u{0638}"), // ظ
        0xFEC9..=0xFECC => Some("\u{0639}"), // ع
        0xFECD..=0xFED0 => Some("\u{063A}"), // غ
        0xFED1..=0xFED4 => Some("\u{0641}"), // ف
        0xFED5..=0xFED8 => Some("\u{0642}"), // ق
        0xFED9..=0xFEDC => Some("\u{0643}"), // ك
        0xFEDD..=0xFEE0 => Some("\u{0644}"), // ل
        0xFEE1..=0xFEE4 => Some("\u{0645}"), // م
        0xFEE5..=0xFEE8 => Some("\u{0646}"), // ن
        0xFEE9..=0xFEEC => Some("\u{0647}"), // ه
        0xFEED..=0xFEEE => Some("\u{0648}"), // و
        0xFEEF..=0xFEF0 => Some("\u{0649}"), // ى
        0xFEF1..=0xFEF4 => Some("\u{064A}"), // ي
        // Lam-Alef ligatures (U+FEF5..=U+FEFC)
        0xFEF5..=0xFEF6 => Some("\u{0644}\u{0622}"), // لآ
        0xFEF7..=0xFEF8 => Some("\u{0644}\u{0623}"), // لأ
        0xFEF9..=0xFEFA => Some("\u{0644}\u{0625}"), // لإ
        0xFEFB..=0xFEFC => Some("\u{0644}\u{0627}"), // لا
        // Zero-width BOM when encountered mid-stream in legacy PDFs
        0xFEFF => Some(""),
        // Arabic Presentation Forms-A: Extended Arabic / Persian / Urdu letters & common ligatures
        0xFB50..=0xFB51 => Some("\u{0671}"), // ٱ
        0xFB52..=0xFB55 => Some("\u{067B}"), // ٻ
        0xFB56..=0xFB59 => Some("\u{067E}"), // پ
        0xFB5A..=0xFB5D => Some("\u{0680}"), // ڀ
        0xFB5E..=0xFB61 => Some("\u{067A}"), // ٺ
        0xFB62..=0xFB65 => Some("\u{067F}"), // ٿ
        0xFB66..=0xFB69 => Some("\u{0679}"), // ٹ
        0xFB6A..=0xFB6D => Some("\u{06A4}"), // ڤ
        0xFB6E..=0xFB71 => Some("\u{06A6}"), // ڦ
        0xFB72..=0xFB75 => Some("\u{0684}"), // ڄ
        0xFB76..=0xFB79 => Some("\u{0683}"), // ڃ
        0xFB7A..=0xFB7D => Some("\u{0686}"), // چ
        0xFB7E..=0xFB81 => Some("\u{0687}"), // ڇ
        0xFB82..=0xFB83 => Some("\u{068D}"), // ڍ
        0xFB84..=0xFB85 => Some("\u{068C}"), // ڌ
        0xFB86..=0xFB87 => Some("\u{068E}"), // ڎ
        0xFB88..=0xFB89 => Some("\u{0688}"), // ڈ
        0xFB8A..=0xFB8B => Some("\u{0698}"), // ژ
        0xFB8C..=0xFB8D => Some("\u{0691}"), // ڑ
        0xFB8E..=0xFB91 => Some("\u{06A9}"), // ک
        0xFB92..=0xFB95 => Some("\u{06AF}"), // گ
        0xFB96..=0xFB99 => Some("\u{06B3}"), // ڳ
        0xFB9A..=0xFB9D => Some("\u{06B1}"), // ڱ
        0xFB9E..=0xFB9F => Some("\u{06BA}"), // ں
        0xFBA0..=0xFBA3 => Some("\u{06BB}"), // ڻ
        0xFBA4..=0xFBA5 => Some("\u{06C0}"), // ۀ
        0xFBA6..=0xFBA9 => Some("\u{06C1}"), // ہ
        0xFBAA..=0xFBAD => Some("\u{06BE}"), // ھ
        0xFBAE..=0xFBAF => Some("\u{06D2}"), // ے
        0xFBB0..=0xFBB1 => Some("\u{06D3}"), // ۓ
        0xFBD3..=0xFBD6 => Some("\u{06AD}"), // ڭ
        0xFBD7..=0xFBD8 => Some("\u{06C7}"), // ۇ
        0xFBD9..=0xFBDA => Some("\u{06C6}"), // ۆ
        0xFBDB..=0xFBDC => Some("\u{06C8}"), // ۈ
        0xFBDD => Some("\u{0677}"),          // ٷ
        0xFBDE..=0xFBDF => Some("\u{06CB}"), // ۋ
        0xFBE0..=0xFBE1 => Some("\u{06C5}"), // ۅ
        0xFBE2..=0xFBE3 => Some("\u{06C9}"), // ۉ
        0xFBE4..=0xFBE7 => Some("\u{06D0}"), // ې
        0xFBE8..=0xFBE9 => Some("\u{0649}"), // ى
        0xFBEA..=0xFBEB => Some("\u{0626}\u{0627}"),
        0xFBEC..=0xFBED => Some("\u{0626}\u{06D5}"),
        0xFBEE..=0xFBEF => Some("\u{0626}\u{0648}"),
        0xFBF0..=0xFBF1 => Some("\u{0626}\u{06C7}"),
        0xFBF2..=0xFBF3 => Some("\u{0626}\u{06C6}"),
        0xFBF4..=0xFBF5 => Some("\u{0626}\u{06C8}"),
        0xFBF6..=0xFBF8 => Some("\u{0626}\u{06D0}"),
        0xFBF9..=0xFBFB => Some("\u{0626}\u{064A}"),
        0xFBFC..=0xFBFF => Some("\u{06CC}"),                // ی
        0xFDF2 => Some("\u{0627}\u{0644}\u{0644}\u{0647}"), // ﷲ -> الله
        0xFDF3 => Some("\u{0627}\u{0643}\u{0628}\u{0631}"),
        0xFDF4 => Some("\u{0645}\u{062D}\u{0645}\u{062F}"),
        0xFDF5 => Some("\u{0635}\u{0644}\u{0639}\u{0645}"),
        0xFDF6 => Some("\u{0631}\u{0633}\u{0648}\u{0644}"),
        0xFDF7 => Some("\u{0639}\u{0644}\u{064A}\u{0647}"),
        0xFDF8 => Some("\u{0648}\u{0633}\u{0644}\u{0645}"),
        0xFDF9 => Some("\u{0635}\u{0644}\u{0649}"),
        0xFDFA => Some("\u{0635}\u{0644}\u{0649} \u{0627}\u{0644}\u{0644}\u{0647} \u{0639}\u{0644}\u{064A}\u{0647} \u{0648}\u{0633}\u{0644}\u{0645}"),
        0xFDFB => Some("\u{062C}\u{0644} \u{062C}\u{0644}\u{0627}\u{0644}\u{0647}"),
        0xFDFC => Some("\u{0631}\u{064A}\u{0627}\u{0644}"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use designcraft_compose::Cache;
    use designcraft_doc::build::NewDocument;
    use designcraft_doc::{Align, Asset, AssetId, Content, Document, Graphic, Item, ItemId, ParaAttrs, ParaFormat, Shape, SpreadRef, TextDirection};
    use designcraft_geom::{Affine as DcAffine, Rect as DcRect, shapes};

    use super::*;
    use crate::{PdfOptions, export_pdf};

    const ARABIC_PROSE_FONT: &str = "DC Test Extract Arabic";

    fn ensure_arabic_prose_font() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let arabic_chars = ['ب', 'س', 'م', 'ا', 'ل', 'ه', 'ر', 'ح', 'ن', 'ي', 'ت', 'ص', 'ف', 'إ', 'أ', 'آ', 'ة', 'و', 'ك', 'د', 'ع', '\u{0640}'];
            let mut map: Vec<(char, char)> = arabic_chars.iter().copied().zip(('A'..='Z').chain('a'..='z')).collect();
            map.push(('—', '-'));
            map.push((' ', ' '));
            map.extend(('0'..='9').map(|d| (d, d)));
            let font = designcraft_fonts::testing::font_mapping(ARABIC_PROSE_FONT, &map).expect("test Arabic font");
            designcraft_fonts::FontDb::global().add_font(font);
        });
    }

    #[test]
    fn extracts_multi_page_english_pdf_preserving_lines_and_words() {
        let mut doc = Document::new(&NewDocument { pages: 2, facing_pages: false, ..NewDocument::default() });
        let lid = doc.default_layer();
        doc.add_text_frame(
            SpreadRef::Doc(0),
            DcRect::new(36.0, 36.0, 540.0, 300.0),
            lid,
            "Hello DesignCraft World\nSecond line of page one",
            ParaFormat::default(),
        )
        .unwrap();
        doc.add_text_frame(
            SpreadRef::Doc(1),
            DcRect::new(36.0, 36.0, 540.0, 300.0),
            lid,
            "Chapter Two Summary\nClean coordinate extraction",
            ParaFormat::default(),
        )
        .unwrap();

        let bytes = export_pdf(&doc, &Cache::new(), &PdfOptions::default()).unwrap();
        let report = extract_text_pages(&bytes).unwrap();

        assert_eq!(report.page_count, 2);
        assert_eq!(report.pages.len(), 2);
        assert!(!report.has_scanned_pages);
        assert_eq!(report.pages[0].page_number, 1);
        assert!(report.pages[0].width_pt > 500.0 && report.pages[0].height_pt > 700.0);
        assert_eq!(report.pages[0].text, "Hello DesignCraft World\nSecond line of page one");
        assert_eq!(report.pages[1].page_number, 2);
        assert_eq!(report.pages[1].text, "Chapter Two Summary\nClean coordinate extraction");
        assert!(report.combined_text.contains("--- Page 1 ---\nHello DesignCraft World"));
        assert!(report.combined_text.contains("\n\n--- Page 2 ---\nChapter Two Summary"));
    }

    #[test]
    fn extracts_arabic_rtl_pdf_preserving_connected_words_and_reading_order() {
        ensure_arabic_prose_font();
        let line1 = "بسم الله الرحمن الرحيم — تصميم احترافي";
        let line2 = "بامتياز عربي متكامل";
        // Ensure all characters in line2 are in our test font or use words from our mapped alphabet:
        let line2_mapped = "تصميم احترافي للكتاب";
        let full_text = format!("{line1}\n{line2_mapped}");
        let _ = line2;

        let mut doc = Document::new(&NewDocument { pages: 1, facing_pages: false, ..NewDocument::default() });
        doc.settings.right_to_left_binding = true;
        let lid = doc.default_layer();
        let para = ParaAttrs { direction: Some(TextDirection::RightToLeft), align: Some(Align::Right), ..ParaAttrs::default() };
        let (_, sid) = doc
            .add_text_frame(SpreadRef::Doc(0), DcRect::new(36.0, 36.0, 560.0, 300.0), lid, &full_text, ParaFormat { para, ..ParaFormat::default() })
            .unwrap();
        doc.story_mut(sid).unwrap().format_chars(0..full_text.len(), |f| {
            f.over.font_family = Some(ARABIC_PROSE_FONT.into());
            f.over.size = Some(14.0);
        });

        let bytes = export_pdf(&doc, &Cache::new(), &PdfOptions::default()).unwrap();
        let report = extract_text_pages(&bytes).unwrap();
        assert_eq!(report.page_count, 1);
        assert_eq!(report.pages[0].text, full_text);
    }

    #[test]
    fn normalizes_arabic_presentation_forms_and_reverses_visual_ltr_runs() {
        // Direct Presentation Forms-B & Forms-A normalization, including Lam-Alef ligatures.
        let pres =
            "\u{FE91}\u{FEB4}\u{FEE2} \u{FE8D}\u{FEDF}\u{FEDF}\u{FEEA} — \u{FEFB} \u{FEF7} \u{FEF9} \u{FEF5} \u{FDF2} \u{FB56}\u{FB7A}\u{FBFC}";
        assert_eq!(normalize_arabic_presentation_forms(pres), "بسم الله — لا لأ لإ لآ الله پچی");

        // Visual-LTR run of Arabic Presentation Forms (emitted leftmost-first with increasing x):
        // Logical phrase: "لا تصميم" -> words in visual LTR order: leftmost is "ميمصت" ('ﻢ','ﻴ','ﻤ','ﺼ','ﺗ'),
        // then space, then rightmost "لا" ('\u{FEFB}').
        let visual_ltr_glyphs = [
            RawGlyph {
                text: "\u{FEE2}".into(),
                origin: Point::new(100.0, 700.0),
                ink_box: Some(Rect::new(100.0, 692.0, 107.0, 702.0)),
                est_size: 12.0,
            },
            RawGlyph {
                text: "\u{FEF4}".into(),
                origin: Point::new(107.0, 700.0),
                ink_box: Some(Rect::new(107.0, 692.0, 113.0, 702.0)),
                est_size: 12.0,
            },
            RawGlyph {
                text: "\u{FEE4}".into(),
                origin: Point::new(113.0, 700.0),
                ink_box: Some(Rect::new(113.0, 692.0, 120.0, 702.0)),
                est_size: 12.0,
            },
            RawGlyph {
                text: "\u{FEBC}".into(),
                origin: Point::new(120.0, 700.0),
                ink_box: Some(Rect::new(120.0, 692.0, 129.0, 702.0)),
                est_size: 12.0,
            },
            RawGlyph {
                text: "\u{FE97}".into(),
                origin: Point::new(129.0, 700.0),
                ink_box: Some(Rect::new(129.0, 692.0, 135.0, 702.0)),
                est_size: 12.0,
            },
            // No explicit space glyph, just a 6pt horizontal gap before Lam-Alef `\u{FEFB}`!
            RawGlyph {
                text: "\u{FEFB}".into(),
                origin: Point::new(142.0, 700.0),
                ink_box: Some(Rect::new(141.0, 692.0, 150.0, 702.0)),
                est_size: 12.0,
            },
        ];
        assert_eq!(reconstruct_page_text(&visual_ltr_glyphs), "لا تصميم");

        // Logical-RTL run of Arabic Presentation Forms (emitted rightmost-first with decreasing x) stays in logical order:
        let logical_rtl_glyphs = [
            RawGlyph {
                text: "\u{FEFB}".into(),
                origin: Point::new(150.0, 700.0),
                ink_box: Some(Rect::new(141.0, 692.0, 150.0, 702.0)),
                est_size: 12.0,
            },
            RawGlyph {
                text: "\u{FE97}".into(),
                origin: Point::new(135.0, 700.0),
                ink_box: Some(Rect::new(129.0, 692.0, 135.0, 702.0)),
                est_size: 12.0,
            },
            RawGlyph {
                text: "\u{FEBC}".into(),
                origin: Point::new(129.0, 700.0),
                ink_box: Some(Rect::new(120.0, 692.0, 129.0, 702.0)),
                est_size: 12.0,
            },
            RawGlyph {
                text: "\u{FEE4}".into(),
                origin: Point::new(120.0, 700.0),
                ink_box: Some(Rect::new(113.0, 692.0, 120.0, 702.0)),
                est_size: 12.0,
            },
            RawGlyph {
                text: "\u{FEF4}".into(),
                origin: Point::new(113.0, 700.0),
                ink_box: Some(Rect::new(107.0, 692.0, 113.0, 702.0)),
                est_size: 12.0,
            },
            RawGlyph {
                text: "\u{FEE2}".into(),
                origin: Point::new(107.0, 700.0),
                ink_box: Some(Rect::new(100.0, 692.0, 107.0, 702.0)),
                est_size: 12.0,
            },
        ];
        assert_eq!(reconstruct_page_text(&logical_rtl_glyphs), "لا تصميم");
    }

    #[test]
    fn detects_scanned_image_only_pdf_page() {
        let mut doc = Document::new(&NewDocument { pages: 1, facing_pages: false, ..NewDocument::default() });
        let png_bytes: Vec<u8> = {
            let mut img = image::RgbaImage::new(8, 8);
            for px in img.pixels_mut() {
                *px = image::Rgba([40, 90, 180, 255]);
            }
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Png).unwrap();
            buf.into_inner()
        };
        let aid = AssetId(doc.alloc());
        doc.add_asset(Asset {
            id: aid,
            name: "scan.png".into(),
            mime: "image/png".into(),
            data: Arc::new(png_bytes),
            pixels: Some((8, 8)),
            ..Asset::default()
        });
        let lid = doc.default_layer();
        let item_id = ItemId(doc.alloc());
        let mut item = Item::new(item_id, lid, Shape::Rectangle, shapes::rectangle(DcRect::new(36.0, 36.0, 236.0, 236.0)));
        item.content = Content::Graphic(Graphic {
            layer_options: None,
            asset: aid,
            size: (8.0, 8.0),
            xf: DcAffine::translate((36.0, 36.0)) * DcAffine::scale(25.0),
            clip: None,
            auto_fit: Default::default(),
            fit_align: 4,
            crop: [0.0; 4],
            wrap: Default::default(),
            fill: Default::default(),
        });
        doc.insert_item(SpreadRef::Doc(0), item, None).unwrap();

        let bytes = export_pdf(&doc, &Cache::new(), &PdfOptions::default()).unwrap();
        let report = extract_text_pages(&bytes).unwrap();
        assert_eq!(report.page_count, 1);
        assert!(report.pages[0].image_count >= 1);
        assert!(report.pages[0].is_likely_scanned);
        assert!(report.has_scanned_pages);
    }

    #[test]
    fn rejects_empty_and_corrupt_pdf_bytes_safely() {
        assert!(extract_text_pages(b"").is_err());
        assert!(extract_text_pages(b"not a pdf at all").is_err());
        assert!(extract_text_pages(b"%PDF-1.7\n%%EOF").is_err());
    }
}
