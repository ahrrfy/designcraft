//! Brief expansion, conversational meta-prefix sanitization (`BUG-007`),
//! Creative Diversity Matrix across multi-page campaigns, and `SessionDesignMemory`.

use crate::intent_geometry::{infer_brief_geometry_detail, normalize_intent_text};
use designcraft_ai_types::protocol::{AiSettings, DocumentGeometrySpec, SelectionContext, TaskRequest, TaskTarget};
use designcraft_engine::doc::Document;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Strip conversational command meta-prefixes and leading count qualifiers (`BUG-007`)
/// so visible document titles, card headers, and curriculum headers never echo user imperative phrasing.
pub fn sanitize_visible_title(raw: &str) -> String {
    let first_line = raw.lines().find(|l| !l.trim().is_empty()).unwrap_or(raw).trim();
    if first_line.is_empty() {
        return "Custom Publication".into();
    }

    let mut working = first_line.to_string();
    let conversational_prefixes = [
        "من فضلك ",
        "لو سمحت ",
        "اريد ان تصمم لي ",
        "أريد أن تصمم لي ",
        "رائع ",
        "ممتاز ",
        "جميل ",
        "حسناً ",
        "حسنا ",
        "طيب ",
        "الآن ",
        "الان ",
        "أولاً ",
        "اولا ",
        "ثم ",
        "لو سمحت ",
        "من فضلك ",
        "ممكن تصمم لي ",
        "ممكن تصمم ",
        "اريد منك تصميم ",
        "أريد منك تصميم ",
        "اريد تصميم ",
        "أريد تصميم ",
        "اريد ",
        "أريد ",
        "إريد ",
        "نريد ",
        "ابي ",
        "أبي ",
        "ابغى ",
        "أبغى ",
        "صمم لي ",
        "صمم لنا ",
        "صمم ",
        "اعمل لي ",
        "اعمل لنا ",
        "اعمل ",
        "انشئ لي ",
        "أنشئ لي ",
        "انشئ ",
        "أنشئ ",
        "انتج لي ",
        "أنتج لي ",
        "انتج ",
        "أنتج ",
        "احتاج الى ",
        "أحتاج إلى ",
        "احتاج لي ",
        "احتاج ",
        "أحتاج ",
        "مطلوب تصميم ",
        "مطلوب اعداد ",
        "مطلوب إعداد ",
        "مطلوب ",
        "جهز لي ",
        "جهز ",
        "ابني لي ",
        "great ",
        "awesome ",
        "perfect ",
        "okay ",
        "ok ",
        "now ",
        "next ",
        "first ",
        "please create ",
        "please design ",
        "please make ",
        "please ",
        "can you create ",
        "can you design ",
        "could you create ",
        "i want you to create ",
        "i want you to design ",
        "i want a ",
        "i want an ",
        "i want ",
        "i need a ",
        "i need an ",
        "i need ",
        "create me a ",
        "create me ",
        "create a ",
        "create an ",
        "create ",
        "design me a ",
        "design me ",
        "design a ",
        "design an ",
        "design ",
        "make me a ",
        "make me ",
        "make a ",
        "make an ",
        "make ",
        "build a ",
        "build ",
        "generate a ",
        "generate ",
    ];

    for _ in 0..6 {
        let trimmed = working.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '،' | ',' | '!' | ':' | '؛' | '.'));
        let lower = trimmed.to_lowercase();
        let mut matched_bytes = None;
        for prefix in conversational_prefixes {
            let stem = prefix.trim_end();
            if lower.starts_with(prefix)
                || (lower.starts_with(stem)
                    && lower
                        .get(stem.len()..)
                        .and_then(|r| r.chars().next())
                        .is_some_and(|c| c.is_whitespace() || matches!(c, '،' | ',' | '!' | ':' | '؛' | '.')))
            {
                let char_len = stem.chars().count();
                if let Some((byte_idx, _)) = trimmed.char_indices().nth(char_len) {
                    matched_bytes = Some(byte_idx);
                    break;
                }
            }
        }
        if let Some(byte_idx) = matched_bytes
            && let Some(rest) = trimmed.get(byte_idx..)
        {
            working = rest.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '،' | ',' | '!' | ':' | '؛' | '.')).to_string();
            continue;
        }
        break;
    }

    // Strip leading `<digits> <count-noun> [من/عن/حول/لـ/of/for/about]` (e.g. "6 تصاميم ", "١٠ صفحات كتاب عن ").
    working = strip_leading_count_phrase(&working);

    // Strip leading generic medium wrappers when a richer subject follows.
    let medium_wrappers = [
        "تصاميم سوشيل ميديا لاعلانات ",
        "تصاميم سوشيال ميديا لاعلانات ",
        "سوشيل ميديا لاعلانات ",
        "سوشيال ميديا لاعلانات ",
        "تصاميم سوشيل ميديا عن ",
        "تصاميم سوشيال ميديا عن ",
        "سوشيل ميديا عن ",
        "سوشيال ميديا عن ",
        "تصاميم سوشيل ميديا ",
        "تصاميم سوشيال ميديا ",
        "بوستات سوشيل ميديا ",
        "بوستات انستغرام عن ",
        "انستغرام عن ",
        "كتاب تعليمي عن ",
        "كتاب عن ",
        "كتيب عن ",
        "منهج عن ",
        "بوستر عن ",
        "عرض تقديمي عن ",
        "social media designs for ",
        "social media posts for ",
        "social media ads for ",
        "social media for ",
        "a book about ",
        "book about ",
        "poster for ",
    ];
    let norm_work = normalize_intent_text(&working);
    for wrapper in medium_wrappers {
        let norm_wrap = normalize_intent_text(wrapper);
        if norm_work.starts_with(&norm_wrap) {
            let word_count = wrapper.split_whitespace().count();
            let remaining: Vec<&str> = working.split_whitespace().skip(word_count).collect();
            if !remaining.is_empty() {
                working = remaining.join(" ");
                break;
            }
        }
    }

    for lead_prep in ["عن ", "حول ", "لموضوع ", "بعنوان ", "لـ ", "about ", "for "] {
        let lower = working.to_lowercase();
        if lower.starts_with(lead_prep) {
            let char_len = lead_prep.chars().count();
            if let Some((byte_idx, _)) = working.char_indices().nth(char_len)
                && let Some(rest) = working.get(byte_idx..)
                && !rest.trim().is_empty()
            {
                working = rest.trim().to_string();
            }
        }
    }

    let final_str: String = working.trim().chars().take(80).collect();
    if final_str.is_empty() { first_line.chars().take(80).collect() } else { final_str }
}

fn is_any_digit_token(tok: &str) -> bool {
    let trimmed = tok.trim_matches(|c: char| matches!(c, '(' | ')' | '-' | ':' | '،' | ','));
    !trimmed.is_empty()
        && trimmed.chars().all(|c| c.is_ascii_digit() || ('\u{0660}'..='\u{0669}').contains(&c) || ('\u{06F0}'..='\u{06F9}').contains(&c))
}

fn is_count_noun_token(tok: &str) -> bool {
    let norm = normalize_intent_text(tok.trim_matches(|c: char| !c.is_alphanumeric()));
    let stripped = norm.strip_prefix("ال").unwrap_or(&norm);
    matches!(
        stripped,
        "تصاميم"
            | "تصميمات"
            | "تصميم"
            | "تصميمين"
            | "صفحات"
            | "صفحه"
            | "صفحتين"
            | "اعلانات"
            | "اعلان"
            | "اعلانين"
            | "بوستات"
            | "بوست"
            | "بوستين"
            | "منشورات"
            | "منشور"
            | "ستوريات"
            | "ستوري"
            | "شرائح"
            | "شريحه"
            | "سلايدات"
            | "بطاقات"
            | "كروت"
            | "نسخ"
            | "pages"
            | "page"
            | "designs"
            | "design"
            | "posts"
            | "post"
            | "ads"
            | "ad"
            | "slides"
            | "cards"
            | "banners"
    )
}

fn is_count_qualifier_token(tok: &str) -> bool {
    let norm = normalize_intent_text(tok.trim_matches(|c: char| !c.is_alphanumeric()));
    let stripped = norm.strip_prefix("ال").unwrap_or(&norm);
    matches!(
        stripped,
        "social"
            | "media"
            | "instagram"
            | "facebook"
            | "tiktok"
            | "linkedin"
            | "marketing"
            | "promotional"
            | "digital"
            | "print"
            | "creative"
            | "luxury"
            | "modern"
            | "professional"
            | "new"
            | "different"
            | "distinct"
            | "سوشيل"
            | "سوشيال"
            | "ميديا"
            | "انستغرام"
            | "انستجرام"
            | "اعلاني"
            | "اعلانيه"
            | "تسويقي"
            | "تسويقيه"
            | "احترافيه"
            | "مختلفه"
            | "متنوعه"
            | "جديده"
    )
}

fn strip_leading_count_phrase(text: &str) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }
    let mut skip = 0usize;
    if let Some(first) = words.first() {
        if is_any_digit_token(first) {
            let max_scan = 4usize.min(words.len().saturating_sub(1));
            for idx in 1..=max_scan {
                if let Some(word) = words.get(idx) {
                    if is_count_noun_token(word) {
                        skip = idx + 1;
                        while words.get(skip).is_some_and(|w| is_count_noun_token(w)) {
                            skip += 1;
                        }
                        break;
                    }
                    if !is_count_qualifier_token(word) {
                        break;
                    }
                }
            }
        } else if is_count_noun_token(first)
            && matches!(normalize_intent_text(first).as_str(), "صفحتين" | "تصميمين" | "اعلانين" | "بوستين" | "شريحتين" | "بطاقتين")
        {
            skip = 1;
        }
    }
    if skip > 0 && words.len() > skip {
        if let Some(prep) = words.get(skip) {
            let p_norm = normalize_intent_text(prep);
            if matches!(p_norm.as_str(), "من" | "عن" | "حول" | "لي" | "لنا" | "لاعلانات" | "لإعلانات" | "of" | "for" | "about") && words.len() > skip + 1
            {
                skip += 1;
            }
        }
        return words.get(skip..).map(|s| s.join(" ")).unwrap_or_else(|| text.to_string());
    }
    text.to_string()
}

/// Archetype slot in the Creative Diversity Matrix so multi-page campaigns never produce monotonous clones.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageArchetypeBlueprint {
    pub page_number: u32,
    pub archetype_id: String,
    pub archetype_name: String,
    pub layout_structure: String,
    pub visual_focus: String,
    pub copy_hierarchy: String,
}

const CAMPAIGN_ARCHETYPES: [(&str, &str, &str, &str, &str); 6] = [
    (
        "hero_showcase",
        "Hero Showcase",
        "Bold asymmetric focal hero zone (top/side 58%) + high-impact display headline + accent eyebrow tag + crisp rounded CTA pill",
        "High-contrast primary brand backdrop with subtle geometric accent framing",
        "Eyebrow badge -> Hero Headline (largest optical scale) -> 1-line value proposition -> Primary CTA button",
    ),
    (
        "editorial_split",
        "Editorial Split",
        "Clean 55/45 split composition: structured visual showcase panel paired with editorial narrative column and 3 key highlights",
        "Contrasting light/surface card against deep brand panel with crisp typographic alignment",
        "Section kicker -> Editorial Sub-headline -> 3 scannable highlight bullets -> Secondary brand lockup",
    ),
    (
        "feature_grid",
        "Feature Grid / Tier Comparison",
        "Modular 2x2 or 3-column card matrix comparing tiers/benefits (e.g. Personal vs Commercial vs Corporate packages) with distinct header badges",
        "Structured grid with subtle card borders, tinted header pills, and strong numeric/icon callouts",
        "Matrix Header -> Tier/Feature Cards (Title + Core Spec + Benefit) -> Comparative summary bar",
    ),
    (
        "minimalist_quote",
        "Minimalist Statement / Social Proof",
        "Generous negative space (40%+ breathing room), oversized typographic statement or client trust testimonial, fine architectural rule",
        "Refined editorial calm with accent quotation mark / emblem and restrained luxury palette",
        "Accent emblem -> Statement / Pull-Quote -> Attribution or key metric -> Subtle brand signature",
    ),
    (
        "offer_promo_card",
        "Offer / Action Promo Card",
        "Framed central spotlight card with prominent value/offer callout badge, deliverable checklist, and anchored bottom CTA bar",
        "Energetic accent highlight badge paired with structured dark-on-light or light-on-dark contrast",
        "Spotlight Offer Badge -> Core Headline -> Deliverable Checklist -> Direct Action / Contact Strip",
    ),
    (
        "corporate_story_cta",
        "Corporate Identity & Closing CTA",
        "Executive brand header + 3 capability pillars + full contact/website/QR footer strip for B2B & enterprise conversion",
        "Authoritative corporate grid with balanced geometric dividers and brand emblem lockup",
        "Brand Identity Header -> Executive Promise -> 3 Capability Pillars -> Contact & CTA Footer",
    ),
];

/// Build the per-page Creative Diversity Matrix for `pages` pages and `genre`.
pub fn build_page_blueprints(genre: &str, pages: u32, clean_subject: &str) -> Vec<PageArchetypeBlueprint> {
    let bounded = pages.clamp(1, 64);
    let mut out = Vec::with_capacity(bounded as usize);
    if genre == "business_card" && bounded == 2 {
        out.push(PageArchetypeBlueprint {
            page_number: 1,
            archetype_id: "card_front_brand".into(),
            archetype_name: "Front — Brand Emblem & Primary Identity".into(),
            layout_structure: "Centered or optical-split brand mark, organization name, and tagline on primary brand surface".into(),
            visual_focus: "Clean emblem focus with generous safe margins inside 3 mm bleed".into(),
            copy_hierarchy: format!("{clean_subject} -> Tagline / Specialty"),
        });
        out.push(PageArchetypeBlueprint {
            page_number: 2,
            archetype_id: "card_back_contact".into(),
            archetype_name: "Back — Executive Contact Details & QR".into(),
            layout_structure: "2-zone structured contact grid: Name & Title + Phone/Email/Web/Address + optional QR frame".into(),
            visual_focus: "High-legibility contrast surface with crisp 8–10 pt typography".into(),
            copy_hierarchy: "Person Name & Role -> Direct Contact Lines -> Website / Location".into(),
        });
        return out;
    }

    for idx in 0..bounded {
        let (id, name, structure, visual, hierarchy) = CAMPAIGN_ARCHETYPES[(idx as usize) % CAMPAIGN_ARCHETYPES.len()];
        let cycle = idx / 6;
        let variant_suffix = if cycle == 0 { String::new() } else { format!(" (Series Variant {})", cycle + 1) };
        out.push(PageArchetypeBlueprint {
            page_number: idx + 1,
            archetype_id: id.to_string(),
            archetype_name: format!("{name}{variant_suffix}"),
            layout_structure: structure.to_string(),
            visual_focus: visual.to_string(),
            copy_hierarchy: hierarchy.to_string(),
        });
    }
    out
}

/// Curated harmonious default swatches (`(SwatchName, HexColor)`) per genre when no brand logo overrides them.
pub fn curated_genre_palette(genre: &str) -> Vec<(String, String)> {
    match genre {
        "business_card" | "corporate_profile" => vec![
            ("Brand-Primary".into(), "#0F172A".into()),
            ("Brand-Secondary".into(), "#1E3A8A".into()),
            ("Brand-Accent".into(), "#D97706".into()),
            ("Surface-Soft".into(), "#F8FAFC".into()),
            ("Surface-Muted".into(), "#E2E8F0".into()),
            ("Ink-Muted".into(), "#475569".into()),
        ],
        "certificate" => vec![
            ("Brand-Primary".into(), "#1E293B".into()),
            ("Brand-Secondary".into(), "#0F766E".into()),
            ("Brand-Accent".into(), "#B45309".into()),
            ("Surface-Soft".into(), "#FFFBEB".into()),
            ("Surface-Muted".into(), "#F1F5F9".into()),
            ("Ink-Muted".into(), "#475569".into()),
        ],
        "social_campaign" | "social_story" | "social_banner" | "social_post" => vec![
            ("Brand-Primary".into(), "#111827".into()),
            ("Brand-Secondary".into(), "#1D4ED8".into()),
            ("Brand-Accent".into(), "#F59E0B".into()),
            ("Surface-Soft".into(), "#F8FAFC".into()),
            ("Surface-Card".into(), "#EFF6FF".into()),
            ("Ink-Muted".into(), "#4B5563".into()),
        ],
        _ => vec![
            ("Brand-Primary".into(), "#1E293B".into()),
            ("Brand-Secondary".into(), "#2563EB".into()),
            ("Brand-Accent".into(), "#EA580C".into()),
            ("Surface-Soft".into(), "#F8FAFC".into()),
            ("Surface-Muted".into(), "#E2E8F0".into()),
            ("Ink-Muted".into(), "#475569".into()),
        ],
    }
}

/// Complete structured Art Direction & Editorial Blueprint derived locally from a user request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExpandedBriefBlueprint {
    pub clean_subject: String,
    pub detected_genre: String,
    pub is_rtl: bool,
    pub total_pages: u32,
    pub inferred_geometry: DocumentGeometrySpec,
    pub target_formats: Vec<DocumentGeometrySpec>,
    pub recommended_palette: Vec<(String, String)>,
    pub typography_scale: String,
    pub page_blueprints: Vec<PageArchetypeBlueprint>,
}

/// Expand a [`TaskRequest`] into an [`ExpandedBriefBlueprint`] without any network/provider calls.
pub fn expand_brief_blueprint(request: &TaskRequest, current_doc: Option<&Document>) -> ExpandedBriefBlueprint {
    let clean_subject = sanitize_visible_title(&request.prompt);
    let inferred = infer_brief_geometry_detail(&request.prompt, &request.settings);
    let is_rtl = request
        .settings
        .right_to_left_binding
        .or_else(|| current_doc.map(|d| d.settings.right_to_left_binding))
        .unwrap_or_else(|| crate::facing_pages::auto_direction(&request.prompt) == "rightToLeft");

    let (total_pages, inferred_geometry, mut companion_formats, detected_genre) = match &request.target {
        TaskTarget::NewDocument { pages, geometry } => {
            let genre = inferred.as_ref().map(|i| i.detected_genre).unwrap_or("custom_layout").to_string();
            let companions = inferred.map(|i| i.target_formats).unwrap_or_default();
            (*pages, geometry.clone(), companions, genre)
        }
        TaskTarget::NewA4 { pages } => {
            let genre = inferred.as_ref().map(|i| i.detected_genre).unwrap_or("editorial_book").to_string();
            let geom = crate::intent_geometry::book_a4_spec(is_rtl, request.settings.facing_pages);
            (*pages, geom, Vec::new(), genre)
        }
        TaskTarget::CurrentDocument | TaskTarget::ContinueBook => {
            let pages = current_doc.and_then(|d| u32::try_from(d.page_count()).ok()).or_else(|| inferred.as_ref().map(|i| i.pages)).unwrap_or(1);
            let geom = if let Some(doc) = current_doc {
                DocumentGeometrySpec {
                    width_pt: doc.settings.page_width,
                    height_pt: doc.settings.page_height,
                    intent: match doc.settings.intent {
                        designcraft_engine::doc::Intent::Print => designcraft_ai_types::DocumentIntentKind::Print,
                        designcraft_engine::doc::Intent::Web => designcraft_ai_types::DocumentIntentKind::Web,
                        designcraft_engine::doc::Intent::Mobile => designcraft_ai_types::DocumentIntentKind::Mobile,
                    },
                    units: format!("{:?}", doc.settings.horizontal_units).to_ascii_lowercase(),
                    facing_pages: Some(doc.settings.facing_pages),
                    right_to_left: Some(doc.settings.right_to_left_binding),
                    margins_pt: None,
                    bleed_pt: doc.settings.bleed[0],
                    columns: 1,
                    column_gutter_pt: 12.0,
                    preset_label: "Current Document".into(),
                }
            } else {
                inferred.as_ref().map(|i| i.primary.clone()).unwrap_or_default()
            };
            let genre = inferred.as_ref().map(|i| i.detected_genre).unwrap_or("editorial_layout").to_string();
            let companions = inferred.map(|i| i.target_formats).unwrap_or_default();
            (pages, geom, companions, genre)
        }
    };

    if !request.target_formats.is_empty() {
        companion_formats = request.target_formats.clone();
    }

    let short_edge = inferred_geometry.width_pt.min(inferred_geometry.height_pt);
    let hero_pt = (short_edge * 0.058).clamp(16.0, 72.0).round();
    let sub_pt = (short_edge * 0.032).clamp(11.0, 38.0).round();
    let body_pt = (short_edge * 0.022).clamp(8.5, 24.0).round();
    let typography_scale = format!(
        "Hero Display: ~{hero_pt} pt Bold | Subhead: ~{sub_pt} pt SemiBold | Body/Details: ~{body_pt} pt Regular (direction: {})",
        if is_rtl { "RTL Arabic-first optical alignment" } else { "LTR optical alignment" }
    );

    let recommended_palette = curated_genre_palette(&detected_genre);
    let page_blueprints = build_page_blueprints(&detected_genre, total_pages, &clean_subject);

    ExpandedBriefBlueprint {
        clean_subject,
        detected_genre,
        is_rtl,
        total_pages,
        inferred_geometry,
        target_formats: companion_formats,
        recommended_palette,
        typography_scale,
        page_blueprints,
    }
}

impl ExpandedBriefBlueprint {
    /// Render concise structured Art Direction instructions for prompt/agent injection.
    pub fn to_prompt_section(&self) -> String {
        let mut out = String::with_capacity(1536);
        out.push_str("## Art Direction & Editorial Blueprint\n");
        out.push_str(&format!(
            "- **Clean Subject (NEVER echo user command verbs like 'اريد'/'صمم لي'/'6 تصاميم' on canvas):** {}\n",
            self.clean_subject
        ));
        out.push_str(&format!(
            "- **Genre:** `{}` | **Canvas:** `{}` ({:.1}×{:.1} pt, `{}`) | **Direction:** {}\n",
            self.detected_genre,
            self.inferred_geometry.preset_label,
            self.inferred_geometry.width_pt,
            self.inferred_geometry.height_pt,
            self.inferred_geometry.units,
            if self.is_rtl { "RTL (`rightToLeft`)" } else { "LTR (`leftToRight`)" }
        ));
        out.push_str(&format!("- **Typography Scale:** {}\n", self.typography_scale));
        let palette_line: Vec<String> = self.recommended_palette.iter().map(|(n, hex)| format!("{n}={hex}")).collect();
        out.push_str(&format!("- **Recommended Harmonious Palette (define via `swatch.create` before referencing):** {}\n", palette_line.join(", ")));
        if !self.target_formats.is_empty() {
            let companions: Vec<String> =
                self.target_formats.iter().map(|f| format!("{} ({:.0}×{:.0})", f.preset_label, f.width_pt, f.height_pt)).collect();
            out.push_str(&format!("- **Companion Multi-Format Targets:** {}\n", companions.join(", ")));
        }
        if self.page_blueprints.len() > 1 {
            out.push_str(
                "- **Creative Diversity Matrix (MANDATORY — each page MUST follow its distinct visual archetype, NEVER clone identical layouts):**\n",
            );
            for bp in self.page_blueprints.iter().take(12) {
                out.push_str(&format!(
                    "  - Page {}: **{}** — {} | Hierarchy: {}\n",
                    bp.page_number, bp.archetype_name, bp.layout_structure, bp.copy_hierarchy
                ));
            }
        }
        out
    }

    pub fn to_preview_json(&self, settings: &AiSettings, active_packs: &[String]) -> Value {
        json!({
            "clean_subject": self.clean_subject,
            "detected_genre": self.detected_genre,
            "binding": if self.is_rtl { "RTL" } else { "LTR" },
            "total_pages": self.total_pages,
            "inferred_geometry": self.inferred_geometry,
            "target_formats": self.target_formats,
            "recommended_palette": self.recommended_palette,
            "typography_scale": self.typography_scale,
            "page_blueprints": self.page_blueprints,
            "active_knowledge_packs": active_packs,
            "execution_mode_plan": {
                "mode": settings.execution_mode,
                "primary_text_model": settings.text_model,
                "hybrid_local_model": settings.hybrid_local_model,
                "multi_format_mode": settings.multi_format_mode,
            }
        })
    }
}

/// Persistent in-memory design memory keyed by `document_uid` across multi-turn edits (`D13`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionDesignMemory {
    pub document_uid: u64,
    pub detected_genre: String,
    pub clean_subject: String,
    pub brand_swatches: Vec<(String, String)>,
    pub primary_font: Option<String>,
    pub secondary_font: Option<String>,
    pub active_knowledge_pack_ids: Vec<String>,
    /// Semantic `object.label` -> `(0-based spread_index, item_id)` harvested from the live document.
    pub labeled_items: BTreeMap<String, (usize, u64)>,
    pub recent_turn_notes: Vec<String>,
}

impl SessionDesignMemory {
    pub fn new(document_uid: u64) -> Self {
        Self { document_uid, ..Default::default() }
    }

    /// Harvest live swatches, fonts, and semantic `item.label` bindings from `doc`.
    pub fn harvest_from_document(
        &mut self,
        document_uid: u64,
        doc: &Document,
        genre: Option<&str>,
        clean_subject: Option<&str>,
        knowledge_pack_ids: &[String],
        turn_note: Option<&str>,
    ) {
        self.document_uid = document_uid;
        if let Some(g) = genre.filter(|g| !g.trim().is_empty()) {
            self.detected_genre = g.to_string();
        }
        if let Some(subj) = clean_subject.filter(|s| !s.trim().is_empty()) {
            self.clean_subject = subj.to_string();
        }
        if !knowledge_pack_ids.is_empty() {
            self.active_knowledge_pack_ids = knowledge_pack_ids.iter().take(16).cloned().collect();
        }

        // Harvest custom document swatches (skipping built-in bracketed swatches like `[Paper]`, `[Black]`).
        let mut custom_swatches = Vec::new();
        for sw in doc.swatches.iter().take(48) {
            if !sw.name.starts_with('[') {
                custom_swatches.push((sw.name.clone(), format!("{:?}", sw.value)));
            }
        }
        if !custom_swatches.is_empty() {
            self.brand_swatches = custom_swatches;
        }

        // Harvest primary & secondary font families from paragraph styles.
        let mut seen_fonts: Vec<String> = Vec::new();
        for style in &doc.styles.paragraph {
            if let Some(fam) = &style.chars.font_family
                && !fam.trim().is_empty()
                && !seen_fonts.iter().any(|f| f.eq_ignore_ascii_case(fam))
            {
                seen_fonts.push(fam.clone());
                if seen_fonts.len() >= 2 {
                    break;
                }
            }
        }
        if let Some(first) = seen_fonts.first() {
            self.primary_font = Some(first.clone());
        }
        if let Some(second) = seen_fonts.get(1) {
            self.secondary_font = Some(second.clone());
        }

        // Harvest semantic labels -> (spread_idx, item_id).
        self.labeled_items.clear();
        for (spread_idx, spread) in doc.spreads.iter().enumerate() {
            for item in &spread.items {
                let lbl = item.label.trim();
                if !lbl.is_empty() && self.labeled_items.len() < 256 {
                    self.labeled_items.insert(lbl.to_string(), (spread_idx, item.id.0));
                }
            }
        }

        if let Some(note) = turn_note.map(str::trim).filter(|n| !n.is_empty()) {
            let bounded: String = note.chars().take(200).collect();
            self.recent_turn_notes.push(bounded);
            if self.recent_turn_notes.len() > 8 {
                let drop_n = self.recent_turn_notes.len().saturating_sub(8);
                self.recent_turn_notes.drain(0..drop_n);
            }
        }
    }

    /// Format a compact multi-turn memory + active selection summary for prompt/agent injection.
    pub fn format_context_summary(&self, selection: Option<&SelectionContext>) -> String {
        let mut out = String::new();
        if !self.detected_genre.is_empty()
            || !self.brand_swatches.is_empty()
            || self.primary_font.is_some()
            || !self.labeled_items.is_empty()
            || !self.recent_turn_notes.is_empty()
        {
            out.push_str("## Session Design Memory (Preserve Visual Identity Across Turns)\n");
            if !self.detected_genre.is_empty() {
                out.push_str(&format!("- Active Genre: `{}`", self.detected_genre));
                if !self.clean_subject.is_empty() {
                    out.push_str(&format!(" | Subject: {}", self.clean_subject));
                }
                out.push('\n');
            }
            if !self.brand_swatches.is_empty() {
                let sw_list: Vec<String> = self.brand_swatches.iter().take(12).map(|(n, _)| format!("`{n}`")).collect();
                out.push_str(&format!("- Established Brand Swatches: {}\n", sw_list.join(", ")));
            }
            if let Some(primary) = &self.primary_font {
                out.push_str(&format!("- Primary Font: `{primary}`"));
                if let Some(sec) = &self.secondary_font {
                    out.push_str(&format!(" | Secondary Font: `{sec}`"));
                }
                out.push('\n');
            }
            if !self.labeled_items.is_empty() {
                let sample: Vec<String> =
                    self.labeled_items.iter().take(24).map(|(lbl, (sp, id))| format!("`{lbl}` -> spread {sp}, item {id}")).collect();
                out.push_str(&format!("- Labeled Canvas Objects: {}\n", sample.join(", ")));
            }
            if !self.recent_turn_notes.is_empty() {
                out.push_str(&format!("- Prior Turn History: {}\n", self.recent_turn_notes.join(" -> ")));
            }
        }

        if let Some(sel) = selection
            && (sel.active_page.is_some()
                || !sel.selected_item_ids.is_empty()
                || sel.selected_story_id.is_some()
                || sel.selected_text_snippet.is_some())
        {
            out.push_str("## Scoped User Selection Context (Prioritize Surgical Edits Here When Requested)\n");
            if let Some(page) = sel.active_page {
                out.push_str(&format!("- Active Page (0-based): {page} (Page {})\n", page + 1));
            }
            if !sel.selected_item_ids.is_empty() {
                out.push_str(&format!("- Selected Item IDs: {:?}\n", sel.selected_item_ids));
            }
            if !sel.selected_labels.is_empty() {
                out.push_str(&format!("- Selected Labels: {:?}\n", sel.selected_labels));
            }
            if !sel.selected_kinds.is_empty() {
                out.push_str(&format!("- Selected Kinds: {:?}\n", sel.selected_kinds));
            }
            if let Some(story_id) = sel.selected_story_id {
                out.push_str(&format!("- Selected Story ID: {story_id}\n"));
            }
            if let Some(snippet) = &sel.selected_text_snippet {
                let clean: String = snippet.chars().take(240).collect();
                out.push_str(&format!("- Selected Text Snippet: \"{clean}\"\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_arabic_and_english_conversational_meta_prefixes_and_counts() {
        assert_eq!(sanitize_visible_title("اريد 6 تصاميم سوشيل ميديا لاعلانات قهوة مختصة فاخرة"), "قهوة مختصة فاخرة");
        assert_eq!(sanitize_visible_title("صمم لي ١٠ صفحات كتاب عن الفضاء والكواكب"), "الفضاء والكواكب");
        assert_eq!(sanitize_visible_title("Please create 6 social media ads for Artisan Roastery"), "Artisan Roastery");
        assert_eq!(sanitize_visible_title("احتاج بطاقة عمل لشركة المدار للهندسة"), "بطاقة عمل لشركة المدار للهندسة");
        assert!(!sanitize_visible_title("رائع الآن صمم لي منشور انستغرام مربع 1080×1080 لمقهى قهوة مختصة").starts_with("رائع"));
    }

    #[test]
    fn builds_six_distinct_creative_diversity_archetypes() {
        let bps = build_page_blueprints("social_campaign", 6, "قهوة مختصة");
        assert_eq!(bps.len(), 6);
        let ids: std::collections::BTreeSet<_> = bps.iter().map(|b| b.archetype_id.as_str()).collect();
        assert_eq!(ids.len(), 6, "all 6 campaign pages must have distinct archetypes");
    }
}
