//! Bilingual (Arabic + English) natural-language intent-to-geometry parser and `file.new` builder.
//!
//! Converts user briefs (including Eastern Arabic digits `٠-٩` / `۰-۹`, dual nouns, and
//! multi-format social campaign cues) into validated [`DocumentGeometrySpec`] values and
//! native `file.new` parameter objects.

use designcraft_ai_types::protocol::{AiSettings, DocumentGeometrySpec, DocumentIntentKind};
use serde_json::{Value, json};

const MM_TO_PT: f64 = 72.0 / 25.4;

/// Detailed result of inferring document geometry and page count from a natural-language brief.
#[derive(Clone, Debug, PartialEq)]
pub struct InferredBriefGeometry {
    pub pages: u32,
    pub primary: DocumentGeometrySpec,
    pub target_formats: Vec<DocumentGeometrySpec>,
    pub detected_genre: &'static str,
}

/// Normalize Eastern Arabic (`٠`..=`٩`), Persian (`۰`..=`۹`), multiplication signs (`×`, `✕`),
/// and Arabic diacritics/tatweel/Alef variants for deterministic intent matching.
pub fn normalize_intent_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().saturating_add(8));
    for ch in designcraft_pdf::extract::normalize_arabic_presentation_forms(raw).chars() {
        if matches!(ch, '\u{0610}'..='\u{061a}' | '\u{0640}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06ed}') {
            continue;
        }
        let mapped = match ch {
            '\u{0660}'..='\u{0669}' => char::from_u32(u32::from('0') + (u32::from(ch) - u32::from('\u{0660}'))).unwrap_or('0'),
            '\u{06F0}'..='\u{06F9}' => char::from_u32(u32::from('0') + (u32::from(ch) - u32::from('\u{06F0}'))).unwrap_or('0'),
            'أ' | 'إ' | 'آ' | 'ٱ' => 'ا',
            'ة' => 'ه',
            'ى' => 'ي',
            '×' | '✕' | '✖' | '*' => 'x',
            _ => ch,
        };
        for lc in mapped.to_lowercase() {
            out.push(lc);
        }
    }
    out
}

/// Validate a [`DocumentGeometrySpec`] against engine safety bounds.
pub fn validate_geometry_spec(geometry: &DocumentGeometrySpec) -> Result<(), String> {
    if !geometry.width_pt.is_finite()
        || !geometry.height_pt.is_finite()
        || !(36.0..=15_552.0).contains(&geometry.width_pt)
        || !(36.0..=15_552.0).contains(&geometry.height_pt)
    {
        return Err("Document geometry dimensions must be finite values between 36 pt and 15,552 pt".into());
    }
    if !geometry.bleed_pt.is_finite() || !(0.0..=72.0).contains(&geometry.bleed_pt) {
        return Err("Document bleed must be a finite value between 0 pt and 72 pt".into());
    }
    if !(1..=20).contains(&geometry.columns) {
        return Err("Document columns must be between 1 and 20".into());
    }
    if !geometry.column_gutter_pt.is_finite() || !(0.0..=288.0).contains(&geometry.column_gutter_pt) {
        return Err("Document column gutter must be a finite value between 0 pt and 288 pt".into());
    }
    if geometry.units.len() > 32 || geometry.preset_label.len() > 128 {
        return Err("Document geometry unit or preset label is too long".into());
    }
    if let Some(margins) = geometry.margins_pt
        && (margins.iter().any(|m| !m.is_finite() || *m < 0.0)
            || margins[0] + margins[1] >= geometry.height_pt
            || margins[2] + margins[3] >= geometry.width_pt)
    {
        return Err("Document margins must be non-negative and leave positive interior page area".into());
    }
    Ok(())
}

fn canonical_engine_units(units: &str, intent: DocumentIntentKind) -> &'static str {
    match units.trim().to_ascii_lowercase().as_str() {
        "px" | "pixel" | "pixels" => "pixels",
        "mm" | "millimeter" | "millimeters" => "millimeters",
        "cm" | "centimeter" | "centimeters" => "centimeters",
        "in" | "inch" | "inches" => "inches",
        "pt" | "point" | "points" => "points",
        "picas" | "pica" => "picas",
        "ciceros" | "cicero" => "ciceros",
        _ => match intent {
            DocumentIntentKind::Print => "millimeters",
            DocumentIntentKind::Web | DocumentIntentKind::Mobile => "pixels",
        },
    }
}

/// Convert a [`DocumentGeometrySpec`] into a complete `file.new` JSON parameter object.
///
/// Includes a baseline built-in `"preset"` (`"A4"` for print, `"Web"` for digital) so `file.new`
/// preserves the explicit `"units"` instead of overwriting them with application default units.
pub fn geometry_to_file_new_params(geometry: &DocumentGeometrySpec, pages: u32, settings: &AiSettings, prompt: &str) -> Result<Value, String> {
    if !(1..=1000).contains(&pages) {
        return Err("AI new documents support 1–1000 pages".into());
    }
    validate_geometry_spec(geometry)?;
    let rtl =
        geometry.right_to_left.or(settings.right_to_left_binding).unwrap_or_else(|| crate::facing_pages::auto_direction(prompt) == "rightToLeft");
    let facing_pages = geometry.facing_pages.unwrap_or(match geometry.intent {
        DocumentIntentKind::Print => settings.facing_pages,
        DocumentIntentKind::Web | DocumentIntentKind::Mobile => false,
    });
    let intent_str = match geometry.intent {
        DocumentIntentKind::Print => "print",
        DocumentIntentKind::Web => "web",
        DocumentIntentKind::Mobile => "mobile",
    };
    let preset_seed = match geometry.intent {
        DocumentIntentKind::Print => "A4",
        DocumentIntentKind::Web => "Web 1920 × 1080",
        DocumentIntentKind::Mobile => "Mobile 1080 × 1920",
    };
    let units = canonical_engine_units(&geometry.units, geometry.intent);
    let default_margin = if matches!(geometry.intent, DocumentIntentKind::Web | DocumentIntentKind::Mobile) {
        (geometry.width_pt.min(geometry.height_pt) * 0.045).clamp(24.0, 64.0).round()
    } else if geometry.width_pt <= 300.0 || geometry.height_pt <= 200.0 {
        14.173 // ~5 mm safe margin for compact cards
    } else {
        36.0
    };
    let [top, bottom, inside, outside] = geometry.margins_pt.unwrap_or([default_margin; 4]);
    Ok(json!({
        "preset": preset_seed,
        "intent": intent_str,
        "units": units,
        "width": (geometry.width_pt * 1000.0).round() / 1000.0,
        "height": (geometry.height_pt * 1000.0).round() / 1000.0,
        "pages": pages,
        "facingPages": facing_pages,
        "binding": if rtl { "rightToLeft" } else { "leftToRight" },
        "columns": geometry.columns,
        "gutter": geometry.column_gutter_pt,
        "margins": {
            "top": top,
            "bottom": bottom,
            "inside": inside,
            "outside": outside,
        },
        "bleed": geometry.bleed_pt,
    }))
}

pub fn social_square_spec(rtl: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: 1080.0,
        height_pt: 1080.0,
        intent: DocumentIntentKind::Web,
        units: "px".into(),
        facing_pages: Some(false),
        right_to_left: Some(rtl),
        margins_pt: Some([48.0, 48.0, 48.0, 48.0]),
        bleed_pt: 0.0,
        columns: 1,
        column_gutter_pt: 24.0,
        preset_label: "Social Square 1080×1080".into(),
    }
}

pub fn social_portrait_spec(rtl: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: 1080.0,
        height_pt: 1350.0,
        intent: DocumentIntentKind::Web,
        units: "px".into(),
        facing_pages: Some(false),
        right_to_left: Some(rtl),
        margins_pt: Some([54.0, 54.0, 48.0, 48.0]),
        bleed_pt: 0.0,
        columns: 1,
        column_gutter_pt: 24.0,
        preset_label: "Social Portrait 1080×1350".into(),
    }
}

pub fn social_story_spec(rtl: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: 1080.0,
        height_pt: 1920.0,
        intent: DocumentIntentKind::Mobile,
        units: "px".into(),
        facing_pages: Some(false),
        right_to_left: Some(rtl),
        margins_pt: Some([96.0, 120.0, 54.0, 54.0]),
        bleed_pt: 0.0,
        columns: 1,
        column_gutter_pt: 24.0,
        preset_label: "Social Story / Reels 1080×1920".into(),
    }
}

pub fn social_landscape_banner_spec(rtl: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: 1920.0,
        height_pt: 1080.0,
        intent: DocumentIntentKind::Web,
        units: "px".into(),
        facing_pages: Some(false),
        right_to_left: Some(rtl),
        margins_pt: Some([56.0, 56.0, 72.0, 72.0]),
        bleed_pt: 0.0,
        columns: 2,
        column_gutter_pt: 32.0,
        preset_label: "Landscape Banner 1920×1080".into(),
    }
}

pub fn linkedin_banner_spec(rtl: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: 1200.0,
        height_pt: 628.0,
        intent: DocumentIntentKind::Web,
        units: "px".into(),
        facing_pages: Some(false),
        right_to_left: Some(rtl),
        margins_pt: Some([40.0, 40.0, 48.0, 48.0]),
        bleed_pt: 0.0,
        columns: 2,
        column_gutter_pt: 24.0,
        preset_label: "Social Link Banner 1200×628".into(),
    }
}

pub fn business_card_spec(rtl: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: (90.0 * MM_TO_PT * 1000.0).round() / 1000.0,  // 255.118 pt
        height_pt: (55.0 * MM_TO_PT * 1000.0).round() / 1000.0, // 155.906 pt
        intent: DocumentIntentKind::Print,
        units: "mm".into(),
        facing_pages: Some(false),
        right_to_left: Some(rtl),
        margins_pt: Some([14.173, 14.173, 14.173, 14.173]),   // 5 mm safe zone
        bleed_pt: (3.0 * MM_TO_PT * 1000.0).round() / 1000.0, // 8.504 pt (3 mm)
        columns: 1,
        column_gutter_pt: 8.504,
        preset_label: "Business Card 90×55 mm".into(),
    }
}

pub fn poster_a3_spec(rtl: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: (297.0 * MM_TO_PT * 1000.0).round() / 1000.0,  // 841.89 pt
        height_pt: (420.0 * MM_TO_PT * 1000.0).round() / 1000.0, // 1190.551 pt
        intent: DocumentIntentKind::Print,
        units: "mm".into(),
        facing_pages: Some(false),
        right_to_left: Some(rtl),
        margins_pt: Some([42.52, 42.52, 42.52, 42.52]), // 15 mm
        bleed_pt: (3.0 * MM_TO_PT * 1000.0).round() / 1000.0,
        columns: 1,
        column_gutter_pt: 18.0,
        preset_label: "Poster A3".into(),
    }
}

pub fn flyer_a5_spec(rtl: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: (148.0 * MM_TO_PT * 1000.0).round() / 1000.0,  // 419.528 pt
        height_pt: (210.0 * MM_TO_PT * 1000.0).round() / 1000.0, // 595.276 pt
        intent: DocumentIntentKind::Print,
        units: "mm".into(),
        facing_pages: Some(false),
        right_to_left: Some(rtl),
        margins_pt: Some([28.346, 28.346, 28.346, 28.346]), // 10 mm
        bleed_pt: (3.0 * MM_TO_PT * 1000.0).round() / 1000.0,
        columns: 1,
        column_gutter_pt: 12.0,
        preset_label: "Flyer A5".into(),
    }
}

pub fn certificate_a4_landscape_spec(rtl: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: (297.0 * MM_TO_PT * 1000.0).round() / 1000.0,  // 841.89 pt
        height_pt: (210.0 * MM_TO_PT * 1000.0).round() / 1000.0, // 595.276 pt
        intent: DocumentIntentKind::Print,
        units: "mm".into(),
        facing_pages: Some(false),
        right_to_left: Some(rtl),
        margins_pt: Some([42.52, 42.52, 42.52, 42.52]),
        bleed_pt: (3.0 * MM_TO_PT * 1000.0).round() / 1000.0,
        columns: 1,
        column_gutter_pt: 14.0,
        preset_label: "Certificate A4 Landscape".into(),
    }
}

pub fn presentation_16_9_spec(rtl: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: 1920.0,
        height_pt: 1080.0,
        intent: DocumentIntentKind::Web,
        units: "px".into(),
        facing_pages: Some(false),
        right_to_left: Some(rtl),
        margins_pt: Some([64.0, 64.0, 80.0, 80.0]),
        bleed_pt: 0.0,
        columns: 2,
        column_gutter_pt: 32.0,
        preset_label: "Presentation 1920×1080".into(),
    }
}

pub fn book_a4_spec(rtl: bool, facing_pages: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: 595.276,
        height_pt: 841.89,
        intent: DocumentIntentKind::Print,
        units: "mm".into(),
        facing_pages: Some(facing_pages),
        right_to_left: Some(rtl),
        margins_pt: Some([48.0, 54.0, 54.0, 42.0]),
        bleed_pt: (3.0 * MM_TO_PT * 1000.0).round() / 1000.0,
        columns: 1,
        column_gutter_pt: 14.0,
        preset_label: "Book A4".into(),
    }
}

pub fn book_b5_spec(rtl: bool, facing_pages: bool) -> DocumentGeometrySpec {
    DocumentGeometrySpec {
        width_pt: (176.0 * MM_TO_PT * 1000.0).round() / 1000.0,  // 498.898 pt
        height_pt: (250.0 * MM_TO_PT * 1000.0).round() / 1000.0, // 708.661 pt
        intent: DocumentIntentKind::Print,
        units: "mm".into(),
        facing_pages: Some(facing_pages),
        right_to_left: Some(rtl),
        margins_pt: Some([42.52, 48.0, 48.0, 36.85]),
        bleed_pt: (3.0 * MM_TO_PT * 1000.0).round() / 1000.0,
        columns: 1,
        column_gutter_pt: 12.0,
        preset_label: "Book / Planner B5".into(),
    }
}

/// Extract requested page / design / slide / card count from normalized bilingual text.
pub fn parse_requested_count(norm: &str) -> Option<u32> {
    // Check Arabic dual nouns first.
    let dual_nouns = [
        "صفحتين",
        "صفحتان",
        "تصميمين",
        "تصميمان",
        "اعلانين",
        "اعلانان",
        "بوستين",
        "بوستان",
        "بطاقتين",
        "بطاقتان",
        "شريحتين",
        "شريحتان",
        "وجهين",
        "وجهان",
    ];
    if dual_nouns.iter().any(|dual| norm.contains(dual)) {
        return Some(2);
    }

    let count_nouns = [
        "تصاميم",
        "تصميمات",
        "تصميم",
        "صفحات",
        "صفحه",
        "اعلانات",
        "اعلان",
        "بوستات",
        "بوست",
        "منشورات",
        "منشور",
        "ستوريات",
        "ستوري",
        "شرائح",
        "شريحه",
        "سلايدات",
        "سلايد",
        "بطاقات",
        "كروت",
        "نسخ",
        "نماذج",
        "pages",
        "page",
        "designs",
        "design",
        "posts",
        "post",
        "ads",
        "slides",
        "slide",
        "cards",
        "banners",
        "creatives",
    ];

    let tokens: Vec<&str> = norm
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '،' | ';' | '؛' | ':' | '(' | ')' | '[' | ']' | '/' | '-'))
        .filter(|t| !t.is_empty())
        .collect();

    for (idx, tok) in tokens.iter().enumerate() {
        if let Ok(n) = tok.parse::<u32>()
            && (1..=1000).contains(&n)
        {
            // Skip `WxH` tokens or numbers immediately followed by dimension units (`mm`, `cm`, `px`, `pt`, `in`).
            let next1 = tokens.get(idx + 1).copied().unwrap_or("");
            let next2 = tokens.get(idx + 2).copied().unwrap_or("");
            if matches!(next1, "mm" | "cm" | "px" | "pt" | "in" | "ملم" | "سم" | "بكسل" | "x") {
                continue;
            }
            let candidates = [next1, next2];
            for cand in candidates {
                let stripped = cand.strip_prefix("ال").unwrap_or(cand);
                if count_nouns.iter().any(|noun| cand == *noun || stripped == *noun) {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn parse_explicit_dimensions(norm: &str, rtl: bool) -> Option<DocumentGeometrySpec> {
    let tokens: Vec<&str> =
        norm.split(|c: char| c.is_whitespace() || matches!(c, ',' | '،' | '(' | ')' | '[' | ']')).filter(|t| !t.is_empty()).collect();
    for (idx, tok) in tokens.iter().enumerate() {
        let Some((lhs, rhs_raw)) = tok.split_once('x') else {
            continue;
        };
        let Ok(w_raw) = lhs.trim().parse::<f64>() else {
            continue;
        };
        let unit_in_token_start = rhs_raw.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(rhs_raw.len());
        let Some(h_part) = rhs_raw.get(..unit_in_token_start) else {
            continue;
        };
        let Ok(h_raw) = h_part.trim().parse::<f64>() else {
            continue;
        };
        if w_raw <= 0.0 || h_raw <= 0.0 {
            continue;
        }
        let trailing_unit = rhs_raw.get(unit_in_token_start..).unwrap_or("").trim();
        let next_unit = if trailing_unit.is_empty() { tokens.get(idx + 1).copied().unwrap_or("") } else { trailing_unit };
        let (scale, units, intent) = match next_unit {
            "mm" | "ملم" => (MM_TO_PT, "mm", DocumentIntentKind::Print),
            "cm" | "سم" => (MM_TO_PT * 10.0, "cm", DocumentIntentKind::Print),
            "in" | "inch" | "بوصه" => (72.0, "in", DocumentIntentKind::Print),
            "pt" | "نقطه" => (1.0, "pt", DocumentIntentKind::Print),
            _ if w_raw >= 300.0 && h_raw >= 200.0 => (1.0, "px", DocumentIntentKind::Web),
            _ => (MM_TO_PT, "mm", DocumentIntentKind::Print),
        };
        let width_pt = w_raw * scale;
        let height_pt = h_raw * scale;
        if !(36.0..=15_552.0).contains(&width_pt) || !(36.0..=15_552.0).contains(&height_pt) {
            continue;
        }
        let bleed_pt = if intent == DocumentIntentKind::Print { (3.0 * MM_TO_PT * 1000.0).round() / 1000.0 } else { 0.0 };
        let spec = DocumentGeometrySpec {
            width_pt: (width_pt * 1000.0).round() / 1000.0,
            height_pt: (height_pt * 1000.0).round() / 1000.0,
            intent,
            units: units.into(),
            facing_pages: Some(false),
            right_to_left: Some(rtl),
            margins_pt: None,
            bleed_pt,
            columns: 1,
            column_gutter_pt: 16.0,
            preset_label: format!("Custom {}×{} {}", w_raw, h_raw, units),
        };
        if validate_geometry_spec(&spec).is_ok() {
            return Some(spec);
        }
    }
    None
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

fn has_multi_size_social_campaign_cues(norm: &str) -> bool {
    let personal_and_corporate = (norm.contains("شخصي") || norm.contains("فردي") || norm.contains("personal"))
        && (norm.contains("تجاري") || norm.contains("شركات") || norm.contains("شركه") || norm.contains("corporate") || norm.contains("commercial"));
    let explicit_multi = contains_any(
        norm,
        &[
            "مقاسات متعدده",
            "احجام متعدده",
            "جميع المقاسات",
            "كل المقاسات",
            "احجام مختلفه",
            "مقاسات مختلفه",
            "multi-size",
            "multi size",
            "multiple sizes",
            "multiple formats",
            "all social sizes",
        ],
    );
    let multi_platform_mix = [
        contains_any(norm, &["ستوري", "ريلز", "story", "stories", "reel", "reels", "tiktok", "تيك توك"]),
        contains_any(norm, &["بوست", "مربع", "square", "feed", "4:5", "1080x1080"]),
        contains_any(norm, &["بنر", "بانر", "يوتيوب", "لينكدان", "لينكد ان", "ترويسه", "banner", "youtube", "linkedin", "landscape"]),
    ]
    .into_iter()
    .filter(|hit| *hit)
    .count()
        >= 2;
    personal_and_corporate || explicit_multi || multi_platform_mix
}

/// Infer `(pages, primary_geometry, companion_target_formats)` from a bilingual natural-language prompt.
pub fn infer_prompt_geometry(prompt: &str, settings: &AiSettings) -> Option<(u32, DocumentGeometrySpec, Vec<DocumentGeometrySpec>)> {
    let detail = infer_brief_geometry_detail(prompt, settings)?;
    Some((detail.pages, detail.primary, detail.target_formats))
}

/// Rich variant of [`infer_prompt_geometry`] that also returns a canonical genre identifier.
pub fn infer_brief_geometry_detail(prompt: &str, settings: &AiSettings) -> Option<InferredBriefGeometry> {
    if prompt.trim().is_empty() {
        return None;
    }
    let norm = normalize_intent_text(prompt);
    let rtl = settings.right_to_left_binding.unwrap_or_else(|| crate::facing_pages::auto_direction(prompt) == "rightToLeft");
    let explicit_count = parse_requested_count(&norm);

    // 1. Business card (`90×55 mm`, 3 mm bleed)
    if contains_any(
        &norm,
        &["بطاقه عمل", "بطاقات عمل", "كرت شخصي", "كروت شخصيه", "كرت عمل", "business card", "business cards", "visiting card", "name card"],
    ) {
        return Some(InferredBriefGeometry {
            pages: explicit_count.unwrap_or(2),
            primary: business_card_spec(rtl),
            target_formats: Vec::new(),
            detected_genre: "business_card",
        });
    }

    // 2. Certificate / Award (`A4 Landscape`)
    if contains_any(&norm, &["شهاده تقدير", "شهاده شكر", "شهاده حضور", "شهاده اجتياز", "شهاده تخرج", "certificate", "diploma", "award certificate"])
    {
        return Some(InferredBriefGeometry {
            pages: explicit_count.unwrap_or(1),
            primary: certificate_a4_landscape_spec(rtl),
            target_formats: Vec::new(),
            detected_genre: "certificate",
        });
    }

    // 3. Presentation / Pitch Deck (`1920×1080 px`, Web)
    if contains_any(&norm, &["عرض تقديمي", "عروض تقديميه", "بوربوينت", "سلايدات", "شرائح عرض", "pitch deck", "slide deck", "presentation", "keynote"])
    {
        return Some(InferredBriefGeometry {
            pages: explicit_count.unwrap_or(6),
            primary: presentation_16_9_spec(rtl),
            target_formats: Vec::new(),
            detected_genre: "presentation",
        });
    }

    // 4. Poster (`A3`)
    if contains_any(&norm, &["بوستر", "ملصق اعلاني", "ملصق جداري", "لوحه اعلانيه", "a3", "poster"]) {
        return Some(InferredBriefGeometry {
            pages: explicit_count.unwrap_or(1),
            primary: poster_a3_spec(rtl),
            target_formats: Vec::new(),
            detected_genre: "poster",
        });
    }

    // 5. Flyer / Handout (`A5`)
    if contains_any(&norm, &["فلاير", "نشره اعلانيه", "مطويه صغيره", "برشور صغير", "a5", "flyer", "handbill", "leaflet"])
    {
        return Some(InferredBriefGeometry {
            pages: explicit_count.unwrap_or(1),
            primary: flyer_a5_spec(rtl),
            target_formats: Vec::new(),
            detected_genre: "flyer",
        });
    }

    // 6. Social Media & Digital Ads (Square / Portrait / Story / Reels / Banner + Multi-size Campaign)
    let is_story = contains_any(&norm, &["ستوري", "ستوريات", "ريلز", "تيك توك", "سناب", "story", "stories", "reel", "reels", "tiktok", "9:16"]);
    let is_landscape_banner = contains_any(
        &norm,
        &["بنر يوتيوب", "بانر يوتيوب", "غلاف يوتيوب", "بنر ويب", "بانر عريض", "youtube banner", "youtube thumbnail", "web banner", "cover banner"],
    );
    let is_linkedin_link = contains_any(&norm, &["لينكدان", "لينكد ان", "بنر لينكدان", "linkedin", "1200x628"]);
    let is_portrait_social = contains_any(&norm, &["بورتريه انستغرام", "1080x1350", "4:5", "instagram portrait", "carousel portrait"]);
    let is_social_general = is_story
        || is_landscape_banner
        || is_linkedin_link
        || is_portrait_social
        || contains_any(
            &norm,
            &[
                "سوشيل ميديا",
                "سوشيال ميديا",
                "مواقع التواصل",
                "وسائل التواصل",
                "منصات التواصل",
                "بوست انستغرام",
                "بوستات انستغرام",
                "انستغرام",
                "انستقرام",
                "فيسبوك",
                "تويتر",
                "اعلانات رقميه",
                "حمله اعلانيه",
                "تصاميم اعلانات",
                "اعلان سوشيل",
                "اعلان سوشيال",
                "social media",
                "instagram",
                "facebook post",
                "social post",
                "social ad",
                "ad creative",
            ],
        );

    if is_social_general {
        let multi_campaign = has_multi_size_social_campaign_cues(&norm);
        if multi_campaign {
            let primary = social_square_spec(rtl);
            let target_formats = vec![social_story_spec(rtl), social_landscape_banner_spec(rtl)];
            return Some(InferredBriefGeometry { pages: explicit_count.unwrap_or(3), primary, target_formats, detected_genre: "social_campaign" });
        }
        let (primary, genre) = if is_story {
            (social_story_spec(rtl), "social_story")
        } else if is_linkedin_link {
            (linkedin_banner_spec(rtl), "social_banner")
        } else if is_landscape_banner {
            (social_landscape_banner_spec(rtl), "social_banner")
        } else if is_portrait_social {
            (social_portrait_spec(rtl), "social_post")
        } else {
            (social_square_spec(rtl), "social_campaign")
        };
        return Some(InferredBriefGeometry { pages: explicit_count.unwrap_or(1), primary, target_formats: Vec::new(), detected_genre: genre });
    }

    // 7. Explicit custom `WxH` dimensions in prompt
    if let Some(custom) = parse_explicit_dimensions(&norm, rtl) {
        return Some(InferredBriefGeometry {
            pages: explicit_count.unwrap_or(1),
            primary: custom,
            target_formats: Vec::new(),
            detected_genre: "custom_layout",
        });
    }

    // 8. Planner / Notebook / Diary (`B5`)
    if contains_any(&norm, &["مفكره يوميه", "مفكره", "اجنده", "منظم يومي", "دفتر ملاحظات", "b5", "daily planner", "planner", "journal"])
    {
        let pages = explicit_count.unwrap_or(8);
        let facing = settings.facing_pages || pages >= 4;
        return Some(InferredBriefGeometry { pages, primary: book_b5_spec(rtl, facing), target_formats: Vec::new(), detected_genre: "planner" });
    }

    // 9. Book / Curriculum / Magazine / Brochure / Catalog (`A4`)
    if contains_any(
        &norm,
        &[
            "كتاب",
            "كتيب",
            "منهج",
            "كراسه",
            "مجله",
            "بروشور",
            "كتالوج",
            "ملف تعريفي",
            "بروفايل شركه",
            "تقرير سنوي",
            "قائمه طعام",
            "منيو",
            "book",
            "booklet",
            "workbook",
            "curriculum",
            "magazine",
            "brochure",
            "catalog",
            "company profile",
            "annual report",
            "menu",
        ],
    ) {
        let is_menu = contains_any(&norm, &["قائمه طعام", "منيو", "menu"]);
        let pages = explicit_count.unwrap_or(if is_menu { 2 } else { 4 });
        let facing = if is_menu { false } else { settings.facing_pages };
        let detected_genre = if is_menu {
            "menu"
        } else if contains_any(&norm, &["منهج", "كراسه", "روضة", "روضه", "اطفال", "workbook", "curriculum", "kindergarten"]) {
            "educational_curriculum"
        } else if contains_any(&norm, &["ملف تعريفي", "بروفايل", "تقرير", "company profile", "annual report", "brochure"]) {
            "corporate_profile"
        } else {
            "editorial_book"
        };
        return Some(InferredBriefGeometry { pages, primary: book_a4_spec(rtl, facing), target_formats: Vec::new(), detected_genre });
    }

    // 10. Generic explicit design/page count fallback (e.g. `"اريد 6 تصاميم اعلانات"`)
    if contains_any(&norm, &["اعلان", "اعلانات", "حمله", "تسويق", "ad ", "ads ", "campaign"]) {
        return Some(InferredBriefGeometry {
            pages: explicit_count.unwrap_or(1),
            primary: social_square_spec(rtl),
            target_formats: Vec::new(),
            detected_genre: "social_campaign",
        });
    }

    if let Some(pages) = explicit_count {
        return Some(InferredBriefGeometry {
            pages,
            primary: book_a4_spec(rtl, settings.facing_pages),
            target_formats: Vec::new(),
            detected_genre: "general_document",
        });
    }

    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parses_arabic_multi_size_social_campaign_with_count_and_companion_formats() {
        let settings = AiSettings::default();
        let prompt = "اريد 6 تصاميم سوشيل ميديا لاعلانات من احجام شخصية واحجام تجارية وشركات";
        let (pages, primary, companions) = infer_prompt_geometry(prompt, &settings).unwrap();
        assert_eq!(pages, 6);
        assert_eq!(primary.width_pt, 1080.0);
        assert_eq!(primary.height_pt, 1080.0);
        assert_eq!(primary.intent, DocumentIntentKind::Web);
        assert_eq!(primary.units, "px");
        assert_eq!(primary.facing_pages, Some(false));
        assert_eq!(primary.right_to_left, Some(true));
        assert_eq!(primary.margins_pt, Some([48.0, 48.0, 48.0, 48.0]));
        assert_eq!(primary.preset_label, "Social Square 1080×1080");
        assert_eq!(companions.len(), 2);
        assert_eq!((companions[0].width_pt, companions[0].height_pt), (1080.0, 1920.0));
        assert_eq!((companions[1].width_pt, companions[1].height_pt), (1920.0, 1080.0));
    }

    #[test]
    fn parses_eastern_arabic_digits_and_dual_nouns() {
        let settings = AiSettings::default();
        let (pages, primary, _) = infer_prompt_geometry("صمم لي ٦ بوستات انستغرام لمتجر قهوة", &settings).unwrap();
        assert_eq!(pages, 6);
        assert_eq!((primary.width_pt, primary.height_pt), (1080.0, 1080.0));

        let (dual_pages, story, _) = infer_prompt_geometry("اريد تصميمين ستوري انستغرام لعروض الصيف", &settings).unwrap();
        assert_eq!(dual_pages, 2);
        assert_eq!((story.width_pt, story.height_pt), (1080.0, 1920.0));
    }

    #[test]
    fn parses_business_card_poster_certificate_and_rtl_book_geometries() {
        let settings = AiSettings::default();

        let (card_pages, card, _) = infer_prompt_geometry("تصميم بطاقة عمل لشركة هندسية وجهين", &settings).unwrap();
        assert_eq!(card_pages, 2);
        assert!((card.width_pt - 255.118).abs() < 0.01);
        assert!((card.height_pt - 155.906).abs() < 0.01);
        assert!((card.bleed_pt - 8.504).abs() < 0.01);
        assert_eq!(card.intent, DocumentIntentKind::Print);
        assert_eq!(card.facing_pages, Some(false));

        let (poster_pages, poster, _) = infer_prompt_geometry("بوستر A3 لمؤتمر الذكاء الاصطناعي", &settings).unwrap();
        assert_eq!(poster_pages, 1);
        assert!((poster.width_pt - 841.89).abs() < 0.02);
        assert!((poster.height_pt - 1190.551).abs() < 0.02);

        let (cert_pages, cert, _) = infer_prompt_geometry("شهادة تقدير للموظف المثالي", &settings).unwrap();
        assert_eq!(cert_pages, 1);
        assert!((cert.width_pt - 841.89).abs() < 0.02);
        assert!((cert.height_pt - 595.276).abs() < 0.02);
        assert_eq!(cert.facing_pages, Some(false));

        let book_settings = AiSettings { facing_pages: true, ..Default::default() };
        let (book_pages, book, _) = infer_prompt_geometry("كتاب ٢٤ صفحة عن تاريخ العمارة الإسلامية", &book_settings).unwrap();
        assert_eq!(book_pages, 24);
        assert_eq!(book.facing_pages, Some(true));
        assert_eq!(book.right_to_left, Some(true));
    }

    #[test]
    fn geometry_to_file_new_params_applies_cleanly_in_engine_session() {
        let settings = AiSettings::default();
        let spec = social_square_spec(true);
        let params = geometry_to_file_new_params(&spec, 6, &settings, "اريد 6 تصاميم سوشيل ميديا").unwrap();
        let mut session = designcraft_engine::Session::new();
        session.execute("file.new", &params).unwrap();
        let doc = session.doc().unwrap();
        assert_eq!(doc.doc.page_count(), 6);
        assert!((doc.doc.settings.page_width - 1080.0).abs() < 0.01);
        assert!((doc.doc.settings.page_height - 1080.0).abs() < 0.01);
        assert!(!doc.doc.settings.facing_pages);
        assert!(doc.doc.settings.right_to_left_binding);
        assert_eq!(doc.doc.settings.horizontal_units, designcraft_geom::Unit::Pixels);
    }
}
