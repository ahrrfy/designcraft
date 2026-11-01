use std::sync::Arc;

use super::{FontDb, FontFace, ScopedFonts};

/// Verified Regular + Bold face pairing for Unicode Arabic typography (supports split-family
/// companions such as `Sultan_medium/ghayaty2020` + `خط سلطاني عريض`, while filtering out legacy
/// non-Unicode symbol/PUA fonts like `Sultan normal` / `Sultan bold`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArabicFontMatch {
    pub family: String,
    pub regular_family: String,
    pub regular_style: String,
    pub bold_family: String,
    pub bold_style: String,
}

const CORE_ARABIC_PROBE: &str = "ابتثجحخدذرزسشصضطظعغفقكلمنهويءأإآةىَُِّْ0123456789";

pub const PREFERRED_ARABIC_FAMILIES: &[&str] = &[
    "Al-Jazeera-Arabic",
    "Cairo",
    "Dubai",
    "Sakkal Majalla",
    "Amiri",
    "Tajawal",
    "IBM Plex Sans Arabic",
    "Readex Pro",
    "Almarai",
    "DIN Next LT W23",
    "Noto Naskh Arabic",
    "Noto Sans Arabic",
    "Droid Arabic Kufi",
    "Noto Kufi Arabic",
    "Sultan_medium/ghayaty2020",
    "Tahoma",
    "Arial",
];

fn covers_core_arabic(face: &FontFace) -> bool {
    !face.is_placeholder() && CORE_ARABIC_PROBE.chars().all(|c| face.covers(c))
}

const ARABIC_ALIAS_TABLE: &[(&[&str], &[&str])] = &[
    (&["الجزيرة", "الجزيره", "جزيرة", "jazeera"], &["Al-Jazeera-Arabic", "Al-Jazeera", "Al Jazeera Arabic", "Al-Jazeera-Arabic-Regular"]),
    (&["سلطان", "sultan"], &["Sultan_medium/ghayaty2020", "خط سلطاني عريض", "Sultan Medium"]),
    (&["القاهرة", "القاهره", "كايرو"], &["Cairo"]),
    (&["أميري", "اميري"], &["Amiri"]),
    (&["دبي"], &["Dubai"]),
    (&["صقال", "مجلة", "مجله", "majalla"], &["Sakkal Majalla"]),
    (&["دين نكست", "din next"], &["DIN Next LT W23", "DIN Next LT Arabic"]),
    (&["تجوال"], &["Tajawal"]),
    (&["المراعي"], &["Almarai"]),
    (&["كوفي", "kufi"], &["Droid Arabic Kufi", "Noto Kufi Arabic", "Reem Kufi"]),
    (&["نوتو", "نسخ", "noto"], &["Noto Naskh Arabic", "Noto Sans Arabic", "Noto Kufi Arabic"]),
    (&["تاهوما"], &["Tahoma"]),
    (&["اريال", "أريال"], &["Arial"]),
];

fn arabic_alias_candidates(query: &str) -> &'static [&'static str] {
    let cleaned: String = query
        .chars()
        .filter(|c| !('\u{064B}'..='\u{0652}').contains(c) && *c != '\u{0670}' && *c != '\u{0640}')
        .collect::<String>()
        .trim()
        .to_lowercase();
    let q = cleaned.trim_start_matches("خط ").trim();
    match q {
        "cairo" => return &["Cairo"],
        "amiri" => return &["Amiri"],
        "dubai" => return &["Dubai"],
        "din" => return &["DIN Next LT W23", "DIN Next LT Arabic"],
        "tajawal" => return &["Tajawal"],
        "almarai" => return &["Almarai"],
        "tahoma" => return &["Tahoma"],
        "arial" => return &["Arial"],
        _ => {}
    }
    for (triggers, candidates) in ARABIC_ALIAS_TABLE {
        if triggers.iter().any(|t| q.contains(t)) {
            return candidates;
        }
    }
    &[]
}

fn looks_like_arabic_candidate_name(name: &str) -> bool {
    if name.chars().any(|c| ('\u{0600}'..='\u{06FF}').contains(&c)) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    const KEYWORDS: &[&str] = &[
        "arabic",
        "naskh",
        "kufi",
        "amiri",
        "cairo",
        "jazeera",
        "dubai",
        "majalla",
        "tajawal",
        "almarai",
        "changa",
        "scheherazade",
        "lateef",
        "reem",
        "aref",
        "harmattan",
        "markazi",
        "readex",
        "din next",
        "sultan",
        "bayan",
        "damascus",
        "geeza",
        "tahoma",
        "arial",
        "andalus",
        "traditional",
        "simplified",
        "urdu",
        "typesetting",
        "diwani",
        "ruqaa",
        "thuluth",
    ];
    KEYWORDS.iter().any(|kw| lower.contains(kw))
}

impl FontDb {
    pub fn resolve_arabic_font(&self, requested: &str) -> Option<ArabicFontMatch> {
        self.scoped(0).resolve_arabic_font(requested)
    }

    pub fn preferred_arabic_font(&self) -> Option<ArabicFontMatch> {
        self.scoped(0).preferred_arabic_font()
    }

    pub fn available_arabic_families(&self) -> Vec<String> {
        self.scoped(0).available_arabic_families()
    }

    pub fn arabic_glyph_fallback(&self, ch: char, bold: bool) -> Option<(String, String)> {
        self.scoped(0).arabic_glyph_fallback(ch, bold)
    }
}

impl ScopedFonts<'_> {
    fn find_companion_bold(&self, canonical: &str, companions: &[&str]) -> Option<(String, String)> {
        for comp in companions {
            if comp.eq_ignore_ascii_case(canonical) {
                continue;
            }
            let Some(comp_canon) = self.canonical_family(comp) else { continue };
            for st in self.styles(&comp_canon) {
                let face = self.face(&comp_canon, &st);
                if !face.italic && face.weight >= 600.0 && covers_core_arabic(&face) {
                    return Some((face.family.clone(), face.style.clone()));
                }
            }
        }
        None
    }

    fn verify_arabic_family(&self, candidate: &str, companions: &[&str], require_distinct_weights: bool) -> Option<ArabicFontMatch> {
        let canonical = self.canonical_family(candidate)?;
        let mut covering: Vec<Arc<FontFace>> =
            self.styles(&canonical).into_iter().map(|st| self.face(&canonical, &st)).filter(|f| covers_core_arabic(f)).collect();
        if covering.is_empty() {
            let default_face = self.face(&canonical, "Regular");
            if covers_core_arabic(&default_face) {
                covering.push(default_face);
            }
        }
        let reg_face = covering
            .iter()
            .filter(|f| !f.italic && f.weight <= 550.0)
            .min_by(|a, b| (a.weight - 400.0).abs().total_cmp(&(b.weight - 400.0).abs()))
            .or_else(|| covering.iter().filter(|f| !f.italic).min_by(|a, b| a.weight.total_cmp(&b.weight)))
            .or_else(|| covering.first())?;

        let own_bold =
            covering.iter().filter(|f| !f.italic && f.weight >= 600.0).min_by(|a, b| (a.weight - 700.0).abs().total_cmp(&(b.weight - 700.0).abs()));

        let (bold_family, bold_style, has_true_bold) = if let Some(b) = own_bold {
            (b.family.clone(), b.style.clone(), true)
        } else if let Some((bf, bs)) = self.find_companion_bold(&canonical, companions) {
            (bf, bs, true)
        } else {
            let heaviest = covering.iter().filter(|f| !f.italic).max_by(|a, b| a.weight.total_cmp(&b.weight)).unwrap_or(reg_face);
            (heaviest.family.clone(), heaviest.style.clone(), false)
        };

        if require_distinct_weights && (reg_face.weight > 550.0 || !has_true_bold) {
            return None;
        }
        Some(ArabicFontMatch {
            family: canonical,
            regular_family: reg_face.family.clone(),
            regular_style: reg_face.style.clone(),
            bold_family,
            bold_style,
        })
    }

    /// Resolves an Arabic font request (English typographic name, localized alias, or Arabic
    /// natural-language name like `"الجزيرة"` / `"سلطان"` / `"القاهرة"`) to verified Unicode
    /// Arabic Regular + Bold faces, rejecting legacy non-Unicode symbol fonts.
    pub fn resolve_arabic_font(&self, requested: &str) -> Option<ArabicFontMatch> {
        let trimmed = requested.trim();
        if trimmed.is_empty() {
            return None;
        }
        let aliases = arabic_alias_candidates(trimmed);
        for &cand in aliases {
            if let Some(m) = self.verify_arabic_family(cand, aliases, false) {
                return Some(m);
            }
        }
        if let Some(m) = self.verify_arabic_family(trimmed, &[], false) {
            return Some(m);
        }
        let needle = trimmed.to_lowercase();
        for fam in self.families() {
            if fam.to_lowercase().contains(&needle)
                && looks_like_arabic_candidate_name(&fam)
                && let Some(m) = self.verify_arabic_family(&fam, &[], false)
            {
                return Some(m);
            }
        }
        None
    }

    /// Selects the highest-priority installed Unicode Arabic textbook family with both Regular
    /// and Bold faces (`Al-Jazeera-Arabic`, `Cairo`, `Dubai`, `Sakkal Majalla`, `Amiri`, …,
    /// falling back to `Tahoma`/`Arial`).
    pub fn preferred_arabic_font(&self) -> Option<ArabicFontMatch> {
        for require_distinct in [true, false] {
            for &cand in PREFERRED_ARABIC_FAMILIES {
                let companions = arabic_alias_candidates(cand);
                if let Some(m) = self.verify_arabic_family(cand, companions, require_distinct) {
                    return Some(m);
                }
            }
        }
        None
    }

    /// Lists available Unicode-verified Arabic font families in textbook-quality order without
    /// opening unrelated CJK/Western font files.
    pub fn available_arabic_families(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for &cand in PREFERRED_ARABIC_FAMILIES {
            let companions = arabic_alias_candidates(cand);
            if let Some(m) = self.verify_arabic_family(cand, companions, false)
                && !out.iter().any(|existing| existing.eq_ignore_ascii_case(&m.family))
            {
                out.push(m.family);
            }
        }
        for fam in self.families() {
            if out.len() >= 28 {
                break;
            }
            if looks_like_arabic_candidate_name(&fam)
                && !out.iter().any(|existing| existing.eq_ignore_ascii_case(&fam))
                && let Some(m) = self.verify_arabic_family(&fam, &[], false)
                && !out.iter().any(|existing| existing.eq_ignore_ascii_case(&m.family))
            {
                out.push(m.family);
            }
        }
        out
    }

    /// Finds a clean covering `(family, style)` for a single character missing from the primary
    /// Arabic face (such as `ﷺ` `U+FDFA`, `✓` `U+2713`, or `—` `U+2014`).
    pub fn arabic_glyph_fallback(&self, ch: char, bold: bool) -> Option<(String, String)> {
        const GLYPH_FALLBACKS: &[&str] = &[
            "Amiri",
            "Sakkal Majalla",
            "DIN Next LT W23",
            "Traditional Arabic",
            "Simplified Arabic",
            "Tahoma",
            "Segoe UI Symbol",
            "Arial",
            "Source Sans 3",
        ];
        let want_style = if bold { "Bold" } else { "Regular" };
        for &cand in GLYPH_FALLBACKS {
            let Some(canon) = self.canonical_family(cand) else { continue };
            for st in [want_style, "Regular"] {
                let face = self.face(&canon, st);
                if !face.is_placeholder() && face.covers(ch) {
                    return Some((face.family.clone(), face.style.clone()));
                }
            }
        }
        self.fallback_for(ch, 0, Some("ar")).map(|f| (f.family.clone(), f.style.clone()))
    }
}
