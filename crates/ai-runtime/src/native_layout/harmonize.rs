//! Tier-1 Output Harmonizer (`D3`) & Brand Asset Palette/Deduplication (`D4`).
//!
//! Runs deterministically before `production.apply` to eliminate model-generated schema and
//! visual regressions without extra LLM round-trips:
//! 1. **Auto-Swatch Synthesis:** Normalizes `swatch.add` -> `swatch.create`, converts raw CSS hex
//!    (`#1E3A8A`), `rgb(...)`, or unknown swatch names into preceding `swatch.create` commands,
//!    and remaps all `$N` / `${N}` step references after command insertion.
//! 2. **Installed Font Fallback:** Maps uninstalled Arabic/Latin font families on `type.char` and
//!    `style.define` to installed faces (`settings.arabic_font` / `FontDb`).
//! 3. **Z-Order Background Harmonizer:** Emits `object.arrange` (`"to": "back"`) when a `>= 85%`
//!    full-page background rectangle is created *after* text frames on the same spread.
//! 4. **Brand Logo & Asset Deduplication + Palette Extraction:** Extracts dominant RGB brand
//!    colors from `BrandLogo` / `ContentAsset` attachments and deduplicates repeated `file.place`
//!    rasters via `"reuseAsset"`.

use base64::Engine as _;
use designcraft_ai_types::EngineAction;
use designcraft_ai_types::protocol::{AttachmentRole, TaskAttachment};
use designcraft_engine::DocState;
use designcraft_engine::file_access;
use designcraft_fonts::FontDb;
use image::{ImageFormat, ImageReader};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::path::Path;

const MAX_LOGO_BYTES: u64 = 16 * 1024 * 1024;
const MAX_LOGO_PIXELS: u64 = 20_000_000;

/// Options controlling how [`harmonize_actions`] transforms a command sequence.
#[derive(Clone, Debug, Default)]
pub struct HarmonizeOptions {
    /// True when `swatch.create` is allowed by the active catalog (false for sealed batch sub-plans).
    pub allow_swatch_synthesis: bool,
    /// Optional preferred Arabic font family from `AiSettings::arabic_font`.
    pub preferred_arabic_font: Option<String>,
    /// True when the prompt/document is predominantly Arabic.
    pub arabic_context: bool,
    /// Optional task attachments (`BrandLogo` / `ContentAsset`) for palette & `file.place` support.
    pub attachments: Vec<TaskAttachment>,
}

/// Extract up to 3 dominant non-background RGB colors (`Brand-Logo-Primary`, `Brand-Logo-Secondary`,
/// `Brand-Logo-Accent`) from attached PNG/JPEG `BrandLogo` or `ContentAsset` images.
pub fn extract_brand_palette_from_attachments(attachments: &[TaskAttachment]) -> Vec<(String, [u8; 3])> {
    let mut colors: Vec<[u8; 3]> = Vec::new();
    for att in attachments.iter().take(8) {
        if !matches!(att.role, AttachmentRole::BrandLogo | AttachmentRole::ContentAsset) {
            continue;
        }
        let clean = designcraft_ai_types::sanitize_user_path(&att.path);
        if clean.is_empty() {
            continue;
        }
        let path = Path::new(&clean);
        let Ok(bytes) = read_attachment_image_bytes(path) else {
            continue;
        };
        for rgb in sample_dominant_rgb(&bytes) {
            if colors.iter().all(|existing| color_distance_sq(*existing, rgb) >= 2_400) {
                colors.push(rgb);
                if colors.len() >= 3 {
                    break;
                }
            }
        }
        if colors.len() >= 3 {
            break;
        }
    }
    let names = ["Brand-Logo-Primary", "Brand-Logo-Secondary", "Brand-Logo-Accent"];
    colors.into_iter().zip(names).map(|(rgb, name)| (name.to_string(), rgb)).collect()
}

fn color_distance_sq(a: [u8; 3], b: [u8; 3]) -> u32 {
    let dr = i32::from(a[0]) - i32::from(b[0]);
    let dg = i32::from(a[1]) - i32::from(b[1]);
    let db = i32::from(a[2]) - i32::from(b[2]);
    (dr * dr + dg * dg + db * db) as u32
}

fn read_attachment_image_bytes(path: &Path) -> Result<Vec<u8>, String> {
    file_access::check_read(path)?;
    if !path.is_absolute() {
        return Err("Image attachment path must be absolute".into());
    }
    let buf = designcraft_engine::io::read_capped(path, MAX_LOGO_BYTES).map_err(|e| e.to_string())?;
    if buf.len() as u64 > MAX_LOGO_BYTES {
        return Err("Image attachment exceeds size limit".into());
    }
    Ok(buf)
}

fn sample_dominant_rgb(bytes: &[u8]) -> Vec<[u8; 3]> {
    let Ok(dim_reader) = ImageReader::new(Cursor::new(bytes)).with_guessed_format() else {
        return Vec::new();
    };
    let Ok((w, h)) = dim_reader.into_dimensions() else {
        return Vec::new();
    };
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_LOGO_PIXELS {
        return Vec::new();
    }
    let Ok(mut reader) = ImageReader::new(Cursor::new(bytes)).with_guessed_format() else {
        return Vec::new();
    };
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let Ok(decoded) = reader.decode() else {
        return Vec::new();
    };
    let thumb = decoded.thumbnail(64, 64).to_rgba8();
    // Quantize into 5-bit per channel buckets, ignoring transparent or near-white/near-black neutral backgrounds.
    let mut buckets: BTreeMap<(u8, u8, u8), (u32, u64, u64, u64)> = BTreeMap::new();
    for px in thumb.pixels() {
        let [r, g, b, a] = px.0;
        if a < 128 {
            continue;
        }
        // Skip near-white paper backgrounds.
        if r > 240 && g > 240 && b > 240 {
            continue;
        }
        let key = (r >> 4, g >> 4, b >> 4);
        let entry = buckets.entry(key).or_insert((0, 0, 0, 0));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = entry.1.saturating_add(u64::from(r));
        entry.2 = entry.2.saturating_add(u64::from(g));
        entry.3 = entry.3.saturating_add(u64::from(b));
    }
    let mut ranked: Vec<(u32, [u8; 3])> = buckets
        .into_values()
        .filter(|(count, _, _, _)| *count >= 2)
        .map(|(count, sr, sg, sb)| {
            let c64 = u64::from(count).max(1);
            let rgb = [(sr / c64) as u8, (sg / c64) as u8, (sb / c64) as u8];
            // Boost chromatic saturation slightly so vivid brand colors rank ahead of neutral grays.
            let max_c = rgb[0].max(rgb[1]).max(rgb[2]);
            let min_c = rgb[0].min(rgb[1]).min(rgb[2]);
            let sat_boost = u32::from(max_c.saturating_sub(min_c)) / 16 + 1;
            (count.saturating_mul(sat_boost), rgb)
        })
        .collect();
    ranked.sort_by_key(|b| std::cmp::Reverse(b.0));
    let mut out = Vec::new();
    for (_, rgb) in ranked {
        if out.iter().all(|prev| color_distance_sq(*prev, rgb) >= 2_400) {
            out.push(rgb);
            if out.len() >= 3 {
                break;
            }
        }
    }
    out
}

/// Harmonize a slice of [`EngineAction`]s in-place before `production.apply`.
pub fn harmonize_actions(actions: &mut Vec<EngineAction>, base: &DocState, options: &HarmonizeOptions) {
    if actions.is_empty() {
        return;
    }

    // 1. Normalize `swatch.add` -> `swatch.create` and `object.arrange` `mode` -> `to`.
    for action in actions.iter_mut() {
        normalize_swatch_and_arrange_aliases(action);
    }

    // 2. Installed font fallback on `type.char` and `style.define`.
    apply_installed_font_fallbacks(actions, base, options);

    // 3. Collect known swatches from `base.doc.swatches` and existing `swatch.create` actions.
    let mut known_swatches: BTreeSet<String> = ["[None]", "[Paper]", "[Black]", "[Registration]"]
        .into_iter()
        .map(str::to_string)
        .chain(base.doc.swatches.iter().map(|s| s.name.clone()))
        .collect();
    for action in actions.iter() {
        if action.command == "swatch.create"
            && let Some(name) = action.params.get("name").and_then(Value::as_str).map(str::trim)
            && !name.is_empty()
        {
            known_swatches.insert(name.to_string());
        }
    }

    let mut synthesized_swatches: Vec<EngineAction> = Vec::new();
    if options.allow_swatch_synthesis {
        // Seed palette from attached `BrandLogo` if present.
        let brand_palette = extract_brand_palette_from_attachments(&options.attachments);
        for (swatch_name, [r, g, b]) in &brand_palette {
            if known_swatches.insert(swatch_name.clone()) {
                synthesized_swatches.push(EngineAction {
                    command: "swatch.create".into(),
                    params: json!({
                        "name": swatch_name,
                        "color": format!("#{r:02X}{g:02X}{b:02X}"),
                    }),
                });
            }
        }

        let mut hex_to_name: BTreeMap<[u8; 3], String> = brand_palette.into_iter().map(|(n, rgb)| (rgb, n)).collect();
        for action in actions.iter_mut() {
            synthesize_missing_swatches_in_action(action, &mut known_swatches, &mut hex_to_name, &mut synthesized_swatches);
        }
    }

    // 4. Determine full-page background rectangles emitted AFTER text frames on the same spread.
    let page_area = (base.doc.settings.page_width * base.doc.settings.page_height).max(1.0);
    let text_referenced_steps = collect_text_referenced_steps(actions);
    let mut seen_text_on_spread: BTreeSet<usize> = BTreeSet::new();
    let mut arrange_back_after_original: BTreeSet<usize> = BTreeSet::new();

    for (orig_idx, action) in actions.iter().enumerate() {
        if matches!(action.command.as_str(), "frame.create" | "shape.rect") {
            let spread = action.params.get("spread").and_then(Value::as_u64).unwrap_or(0) as usize;
            let content = action.params.get("content").and_then(Value::as_str);
            let is_text = action.command == "frame.create"
                && (content == Some("text") || action.params.get("text").is_some() || text_referenced_steps.contains(&orig_idx));
            if is_text {
                seen_text_on_spread.insert(spread);
            } else if seen_text_on_spread.contains(&spread)
                && let Some(area) = rect_area(action.params.get("rect"))
                && area >= 0.85 * page_area
            {
                let has_visible_fill =
                    action.params.get("fill").and_then(Value::as_str).is_some_and(|f| !f.trim().is_empty() && f.trim() != "[None]");
                let is_graphic = action.command == "shape.rect"
                    || content.is_none()
                    || content == Some("graphic")
                    || content == Some("unassigned")
                    || has_visible_fill;
                if is_graphic && !already_arranged_later(actions, orig_idx) {
                    arrange_back_after_original.insert(orig_idx);
                }
            }
        }
    }

    // 5. Rebuild action list with prepended `swatch.create` commands + inserted `object.arrange` commands,
    // computing exact `old_to_new` step-index remapping so every `$N` / `${N.field}` stays 100% valid!
    let prepend_count = synthesized_swatches.len();
    let mut old_to_new = Vec::with_capacity(actions.len());
    let mut next_idx = prepend_count;
    for orig_idx in 0..actions.len() {
        old_to_new.push(next_idx);
        next_idx += 1;
        if arrange_back_after_original.contains(&orig_idx) {
            next_idx += 1;
        }
    }

    if prepend_count > 0 || !arrange_back_after_original.is_empty() {
        let original = std::mem::take(actions);
        let mut rebuilt = Vec::with_capacity(next_idx);
        rebuilt.extend(synthesized_swatches);
        for (orig_idx, mut act) in original.into_iter().enumerate() {
            remap_step_references(&mut act.params, &old_to_new);
            let created_new_idx = old_to_new[orig_idx];
            rebuilt.push(act);
            if arrange_back_after_original.contains(&orig_idx) {
                rebuilt.push(EngineAction {
                    command: "object.arrange".into(),
                    params: json!({
                        "ids": [format!("${created_new_idx}.id")],
                        "to": "back",
                    }),
                });
            }
        }
        *actions = rebuilt;
    }

    // 6. Deduplicate repeated `file.place` assets across pages (`"reuseAsset"`).
    deduplicate_file_place_assets(actions, base, &options.attachments);
}

fn normalize_swatch_and_arrange_aliases(action: &mut EngineAction) {
    if action.command == "swatch.add" {
        action.command = "swatch.create".into();
        if let Some(obj) = action.params.as_object_mut()
            && obj.get("color").is_none()
        {
            if let Some(vals) = obj.remove("values").and_then(|v| v.as_array().cloned()) {
                let space = obj.remove("space").and_then(|v| v.as_str().map(|s| s.to_ascii_lowercase())).unwrap_or_else(|| "rgb".into());
                if space == "rgb" && vals.len() >= 3 {
                    let nums: Vec<f64> = vals.iter().take(3).filter_map(Value::as_f64).collect();
                    if nums.len() == 3 {
                        let unit_scale = nums.iter().all(|v| (0.0..=1.0).contains(v)) && nums.iter().any(|v| v.fract() != 0.0 || *v == 1.0);
                        let to_u8 = |v: f64| -> u8 {
                            if unit_scale { (v * 255.0).round().clamp(0.0, 255.0) as u8 } else { v.round().clamp(0.0, 255.0) as u8 }
                        };
                        obj.insert("color".into(), json!([to_u8(nums[0]), to_u8(nums[1]), to_u8(nums[2])]));
                    }
                } else if space == "cmyk" && vals.len() >= 4 {
                    let nums: Vec<f64> = vals.iter().take(4).filter_map(Value::as_f64).collect();
                    if nums.len() == 4 {
                        let unit_scale = nums.iter().all(|v| (0.0..=1.0).contains(v)) && nums.iter().any(|v| v.fract() != 0.0);
                        let scale = if unit_scale { 100.0 } else { 1.0 };
                        obj.insert(
                            "color".into(),
                            json!({
                                "c": (nums[0] * scale).clamp(0.0, 100.0),
                                "m": (nums[1] * scale).clamp(0.0, 100.0),
                                "y": (nums[2] * scale).clamp(0.0, 100.0),
                                "k": (nums[3] * scale).clamp(0.0, 100.0),
                            }),
                        );
                    }
                }
            } else {
                obj.remove("space");
            }
        }
    } else if action.command == "object.arrange"
        && let Some(obj) = action.params.as_object_mut()
    {
        if obj.get("to").is_none()
            && let Some(mode_val) = obj.remove("mode").or_else(|| obj.remove("direction"))
            && let Some(mode_str) = mode_val.as_str()
        {
            let lower = mode_str.trim().to_ascii_lowercase();
            let mapped = match lower.as_str() {
                "back" | "sendtoback" | "send_to_back" => "back",
                "front" | "bringtofront" | "bring_to_front" => "front",
                "backward" | "sendbackward" | "send_backward" => "backward",
                "forward" | "bringforward" | "bring_forward" => "forward",
                other => other,
            };
            obj.insert("to".into(), json!(mapped));
        }
        if obj.get("ids").is_none()
            && let Some(single_id) = obj.remove("id")
        {
            obj.insert("ids".into(), json!([single_id]));
        }
    }
}

fn apply_installed_font_fallbacks(actions: &mut [EngineAction], base: &DocState, options: &HarmonizeOptions) {
    let scoped = FontDb::global().scoped(base.doc.font_scope);
    let installed_families = scoped.families();
    if installed_families.is_empty() {
        return;
    }

    let arabic_fallback = options
        .preferred_arabic_font
        .as_deref()
        .map(str::trim)
        .filter(|fam| !fam.is_empty() && !scoped.styles(fam).is_empty())
        .map(str::to_string)
        .or_else(|| scoped.preferred_arabic_font().map(|m| m.regular_family))
        .or_else(|| scoped.available_arabic_families().into_iter().next());

    let latin_candidates = ["Inter", "IBM Plex Sans", "Segoe UI", "Arial", "Helvetica Neue", "Liberation Sans", "DejaVu Sans"];
    let latin_fallback =
        latin_candidates.iter().find(|&&fam| !scoped.styles(fam).is_empty()).map(|&s| s.to_string()).or_else(|| installed_families.first().cloned());

    // Detect if plan text contains Arabic characters.
    let plan_has_arabic = options.arabic_context
        || actions
            .iter()
            .any(|a| a.params.get("text").and_then(Value::as_str).is_some_and(|t| t.chars().any(|c| ('\u{0600}'..='\u{06ff}').contains(&c))));

    let resolve_family = |raw_fam: &str, style_hint_arabic: bool| -> Option<String> {
        let trimmed = raw_fam.trim();
        if trimmed.is_empty() || !scoped.styles(trimmed).is_empty() {
            return None;
        }
        // Case-insensitive match against installed families first.
        if let Some(exact_ci) = installed_families.iter().find(|f| f.eq_ignore_ascii_case(trimmed)) {
            return Some(exact_ci.clone());
        }
        let lower = trimmed.to_ascii_lowercase();
        let wants_arabic = style_hint_arabic
            || plan_has_arabic
            || ["arabic", "cairo", "tajawal", "almarai", "amiri", "naskh", "kufi", "ibm plex sans arabic", "readex", "changa", "dubai", "sakkal"]
                .iter()
                .any(|kw| lower.contains(kw));
        if wants_arabic {
            arabic_fallback.clone().or_else(|| latin_fallback.clone())
        } else {
            latin_fallback.clone().or_else(|| arabic_fallback.clone())
        }
    };

    for action in actions.iter_mut() {
        if !matches!(action.command.as_str(), "type.char" | "style.define") {
            continue;
        }
        let style_name_arabic = action
            .params
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n.chars().any(|c| ('\u{0600}'..='\u{06ff}').contains(&c)) || n.to_ascii_lowercase().contains("ar"));
        let dir_rtl =
            action.params.get("attrs").and_then(|a| a.get("direction")).and_then(Value::as_str).is_some_and(|d| d.eq_ignore_ascii_case("rtl"));
        let hint_ar = style_name_arabic || dir_rtl;

        if let Some(obj) = action.params.as_object_mut() {
            if let Some(fam) = obj.get("fontFamily").and_then(Value::as_str).and_then(|f| resolve_family(f, hint_ar)) {
                obj.insert("fontFamily".into(), json!(fam));
            }
            for sub_key in ["attrs", "char"] {
                if let Some(sub) = obj.get_mut(sub_key).and_then(Value::as_object_mut)
                    && let Some(fam) = sub.get("fontFamily").and_then(Value::as_str).and_then(|f| resolve_family(f, hint_ar))
                {
                    sub.insert("fontFamily".into(), json!(fam));
                }
            }
        }
    }
}

fn synthesize_missing_swatches_in_action(
    action: &mut EngineAction,
    known_swatches: &mut BTreeSet<String>,
    hex_to_name: &mut BTreeMap<[u8; 3], String>,
    synthesized: &mut Vec<EngineAction>,
) {
    let mut register = |raw: &str| -> Option<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || known_swatches.contains(trimmed) || trimmed.starts_with('$') {
            return None;
        }
        // Case-insensitive match against existing swatches (e.g. "paper" -> "[Paper]", "none" -> "[None]").
        match trimmed.to_ascii_lowercase().as_str() {
            "none" | "transparent" => return Some("[None]".into()),
            "paper" | "white" | "#ffffff" | "#fff" => return Some("[Paper]".into()),
            "black" | "#000000" | "#000" => return Some("[Black]".into()),
            _ => {}
        }
        if let Some(existing) = known_swatches.iter().find(|s| s.eq_ignore_ascii_case(trimmed)).cloned() {
            return Some(existing);
        }

        if let Some(rgb) = parse_hex_or_inline_rgb(trimmed) {
            if let Some(reused) = hex_to_name.get(&rgb) {
                return Some(reused.clone());
            }
            let name = format!("Color-{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]);
            if known_swatches.insert(name.clone()) {
                synthesized.push(EngineAction {
                    command: "swatch.create".into(),
                    params: json!({
                        "name": name,
                        "color": format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]),
                    }),
                });
            }
            hex_to_name.insert(rgb, name.clone());
            return Some(name);
        }

        // Named semantic / CSS swatch not yet defined in `doc.swatches` (e.g. `"Navy"`, `"Gold"`, `"Brand-Primary"`).
        let rgb = semantic_name_to_rgb(trimmed);
        let safe_name: String = trimmed.chars().take(64).collect();
        if known_swatches.insert(safe_name.clone()) {
            synthesized.push(EngineAction {
                command: "swatch.create".into(),
                params: json!({
                    "name": safe_name,
                    "color": format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]),
                }),
            });
        }
        None // Keep the original `safe_name` in place since we just defined it!
    };

    let Some(obj) = action.params.as_object_mut() else {
        return;
    };
    match action.command.as_str() {
        "frame.create" | "shape.polygon" | "object.style" => {
            for key in ["fill", "stroke"] {
                if let Some(replacement) = obj.get(key).and_then(Value::as_str).and_then(&mut register) {
                    obj.insert(key.into(), json!(replacement));
                }
            }
        }
        "shape.line" => {
            if let Some(replacement) = obj.get("stroke").and_then(Value::as_str).and_then(&mut register) {
                obj.insert("stroke".into(), json!(replacement));
            }
        }
        "object.fill" | "object.stroke" => {
            if let Some(replacement) = obj.get("swatch").and_then(Value::as_str).and_then(&mut register) {
                obj.insert("swatch".into(), json!(replacement));
            }
        }
        "type.char" | "style.define" => {
            for key in ["fill", "stroke", "underlineColor", "shadingColor"] {
                if let Some(replacement) = obj.get(key).and_then(Value::as_str).and_then(&mut register) {
                    obj.insert(key.into(), json!(replacement));
                }
            }
            if let Some(attrs) = obj.get_mut("attrs").and_then(Value::as_object_mut) {
                for key in ["fill", "stroke", "underlineColor", "shadingColor", "borderColor"] {
                    if let Some(replacement) = attrs.get(key).and_then(Value::as_str).and_then(&mut register) {
                        attrs.insert(key.into(), json!(replacement));
                    }
                }
            }
        }
        "table.setCell" => {
            if let Some(replacement) = obj.get("fill").and_then(Value::as_str).and_then(&mut register) {
                obj.insert("fill".into(), json!(replacement));
            }
            if let Some(stroke_obj) = obj.get_mut("stroke").and_then(Value::as_object_mut) {
                for key in ["color", "swatch", "gapColor"] {
                    if let Some(replacement) = stroke_obj.get(key).and_then(Value::as_str).and_then(&mut register) {
                        stroke_obj.insert(key.into(), json!(replacement));
                    }
                }
            }
        }
        _ => {}
    }
}

fn parse_hex_or_inline_rgb(raw: &str) -> Option<[u8; 3]> {
    if let Some(hex) = raw.strip_prefix('#') {
        if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let r = u8::from_str_radix(hex.get(0..2)?, 16).ok()?;
            let g = u8::from_str_radix(hex.get(2..4)?, 16).ok()?;
            let b = u8::from_str_radix(hex.get(4..6)?, 16).ok()?;
            return Some([r, g, b]);
        }
        if hex.len() == 3 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            let r = u8::from_str_radix(hex.get(0..1)?, 16).ok()? * 17;
            let g = u8::from_str_radix(hex.get(1..2)?, 16).ok()? * 17;
            let b = u8::from_str_radix(hex.get(2..3)?, 16).ok()? * 17;
            return Some([r, g, b]);
        }
    }
    let parsed = super::aliases::parse_inline_color(raw)?;
    let [r, g, b] = parsed.to_rgb_uncalibrated();
    Some([
        (f64::from(r.clamp(0.0, 1.0)) * 255.0).round() as u8,
        (f64::from(g.clamp(0.0, 1.0)) * 255.0).round() as u8,
        (f64::from(b.clamp(0.0, 1.0)) * 255.0).round() as u8,
    ])
}

fn semantic_name_to_rgb(name: &str) -> [u8; 3] {
    let lower = name.to_ascii_lowercase();
    if lower.contains("navy") || lower.contains("midnight") || lower == "brand-primary" {
        [15, 23, 42]
    } else if lower.contains("royal") || lower.contains("blue") || lower == "brand-secondary" {
        [30, 64, 175]
    } else if lower.contains("gold") || lower.contains("amber") || lower == "brand-accent" {
        [217, 119, 6]
    } else if lower.contains("teal") || lower.contains("turquoise") {
        [15, 118, 110]
    } else if lower.contains("emerald") || lower.contains("green") || lower.contains("mint") {
        [5, 150, 105]
    } else if lower.contains("crimson") || lower.contains("red") || lower.contains("burgundy") {
        [185, 28, 28]
    } else if lower.contains("coral") || lower.contains("orange") || lower.contains("terracotta") {
        [234, 88, 12]
    } else if lower.contains("purple") || lower.contains("violet") || lower.contains("indigo") {
        [91, 33, 182]
    } else if lower.contains("cream") || lower.contains("ivory") || lower.contains("beige") || lower.contains("sand") {
        [254, 252, 232]
    } else if lower.contains("surface") || lower.contains("light") || lower.contains("soft") || lower.contains("bg") {
        [248, 250, 252]
    } else if lower.contains("slate") || lower.contains("gray") || lower.contains("grey") || lower.contains("muted") {
        [71, 85, 105]
    } else if lower.contains("charcoal") || lower.contains("dark") || lower.contains("ink") {
        [30, 41, 59]
    } else {
        let digest = Sha256::digest(lower.as_bytes());
        [40 + (digest[0] % 160), 40 + (digest[1] % 160), 40 + (digest[2] % 160)]
    }
}

fn collect_text_referenced_steps(actions: &[EngineAction]) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    for action in actions {
        if matches!(action.command.as_str(), "text.insert" | "text.replace" | "type.char" | "type.para")
            && let Some(story_ref) = action.params.get("story").and_then(Value::as_str)
            && let Some(step) = parse_leading_step_ref(story_ref)
        {
            out.insert(step);
        }
    }
    out
}

fn parse_leading_step_ref(s: &str) -> Option<usize> {
    let rest = s.strip_prefix('$')?;
    let rest = rest.strip_prefix('{').unwrap_or(rest);
    let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    rest.get(..end)?.parse::<usize>().ok()
}

fn rect_area(val: Option<&Value>) -> Option<f64> {
    let arr = val?.as_array()?;
    if arr.len() != 4 {
        return None;
    }
    let x0 = arr.first()?.as_f64()?;
    let y0 = arr.get(1)?.as_f64()?;
    let x1 = arr.get(2)?.as_f64()?;
    let y1 = arr.get(3)?.as_f64()?;
    let w = (x1 - x0).abs();
    let h = (y1 - y0).abs();
    (w.is_finite() && h.is_finite()).then_some(w * h)
}

fn already_arranged_later(actions: &[EngineAction], target_idx: usize) -> bool {
    actions.get(target_idx.saturating_add(1)..).into_iter().flatten().any(|act| {
        if act.command != "object.arrange" {
            return false;
        }
        act.params
            .get("ids")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .any(|s| parse_leading_step_ref(s) == Some(target_idx))
    })
}

/// Remap `$N` and `${N...}` step references inside `value` according to `old_to_new`.
pub fn remap_step_references(value: &mut Value, old_to_new: &[usize]) {
    match value {
        Value::String(s) if s.contains('$') => {
            *s = remap_step_string(s, old_to_new);
        }
        Value::String(_) => {}
        Value::Array(arr) => {
            for item in arr {
                remap_step_references(item, old_to_new);
            }
        }
        Value::Object(map) => {
            for val in map.values_mut() {
                remap_step_references(val, old_to_new);
            }
        }
        _ => {}
    }
}

fn remap_step_string(input: &str, old_to_new: &[usize]) -> String {
    let mut out = String::with_capacity(input.len().saturating_add(8));
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let braced = bytes.get(i + 1) == Some(&b'{');
            let digit_start = if braced { i + 2 } else { i + 1 };
            let mut digit_end = digit_start;
            while digit_end < bytes.len() && bytes[digit_end].is_ascii_digit() {
                digit_end += 1;
            }
            if digit_end > digit_start
                && let Some(num_str) = input.get(digit_start..digit_end)
                && let Ok(old_idx) = num_str.parse::<usize>()
                && let Some(&new_idx) = old_to_new.get(old_idx)
            {
                if braced {
                    out.push_str("${");
                } else {
                    out.push('$');
                }
                out.push_str(&new_idx.to_string());
                i = digit_end;
                continue;
            }
        }
        if let Some(rest) = input.get(i..)
            && let Some(ch) = rest.chars().next()
        {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn deduplicate_file_place_assets(actions: &mut [EngineAction], base: &DocState, attachments: &[TaskAttachment]) {
    // Map SHA-256 of existing `base.doc.assets` so we can reuse already-loaded assets immediately.
    let mut existing_by_sha: BTreeMap<[u8; 32], u64> = BTreeMap::new();
    for (&aid, asset) in &base.doc.assets {
        let digest: [u8; 32] = Sha256::digest(asset.data.as_slice()).into();
        existing_by_sha.entry(digest).or_insert(aid.0);
    }

    // Allow approved `BrandLogo` / `ContentAsset` local paths in `file.place` by converting to inline PNG/JPEG base64 once.
    let mut attachment_lookup: BTreeMap<String, String> = BTreeMap::new();
    for att in attachments {
        if matches!(att.role, AttachmentRole::BrandLogo | AttachmentRole::ContentAsset | AttachmentRole::Auto) {
            let clean = designcraft_ai_types::sanitize_user_path(&att.path);
            if !clean.is_empty() {
                attachment_lookup.insert(clean.clone(), clean.clone());
                if let Some(fname) = Path::new(&clean).file_name().and_then(|f| f.to_str()) {
                    attachment_lookup.entry(fname.to_string()).or_insert(clean);
                }
            }
        }
    }

    let mut first_step_by_sha: BTreeMap<[u8; 32], usize> = BTreeMap::new();
    for (step_idx, action) in actions.iter_mut().enumerate() {
        if action.command != "file.place" {
            continue;
        }
        let Some(obj) = action.params.as_object_mut() else {
            continue;
        };
        if obj.get("reuseAsset").is_some() {
            continue;
        }

        // Convert authorized attachment `"path"` to inline `"base64"` PNG/JPEG if `"base64"` is absent.
        if obj.get("base64").is_none()
            && let Some(raw_path) = obj.get("path").and_then(Value::as_str).map(str::to_string)
        {
            let sanitized = designcraft_ai_types::sanitize_user_path(&raw_path);
            let resolved = attachment_lookup.get(&sanitized).or_else(|| attachment_lookup.get(&raw_path)).cloned();
            if let Some(valid_path) = resolved
                && let Ok(raw_bytes) = read_attachment_image_bytes(Path::new(&valid_path))
                && let Some((normalized_bytes, ext_name)) = ensure_png_or_jpeg(&raw_bytes, &valid_path)
            {
                obj.remove("path");
                obj.insert("name".into(), json!(ext_name));
                obj.insert("base64".into(), json!(base64::engine::general_purpose::STANDARD.encode(&normalized_bytes)));
            }
        }

        let Some(b64) = obj.get("base64").and_then(Value::as_str) else {
            continue;
        };
        let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
            continue;
        };
        let digest: [u8; 32] = Sha256::digest(&decoded).into();
        if let Some(&existing_aid) = existing_by_sha.get(&digest) {
            obj.remove("base64");
            obj.insert("reuseAsset".into(), json!(existing_aid));
        } else if let Some(&first_step) = first_step_by_sha.get(&digest) {
            obj.remove("base64");
            obj.insert("reuseAsset".into(), json!(format!("${first_step}.asset")));
        } else {
            first_step_by_sha.insert(digest, step_idx);
        }
    }
}

fn ensure_png_or_jpeg(bytes: &[u8], path_str: &str) -> Option<(Vec<u8>, String)> {
    let stem = Path::new(path_str).file_stem().and_then(|s| s.to_str()).unwrap_or("brand-asset");
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((bytes.to_vec(), format!("{stem}.png")));
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some((bytes.to_vec(), format!("{stem}.jpg")));
    }
    let decoded = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?.decode().ok()?;
    let mut out = Cursor::new(Vec::new());
    decoded.write_to(&mut out, ImageFormat::Png).ok()?;
    Some((out.into_inner(), format!("{stem}.png")))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use designcraft_engine::Session;

    #[test]
    fn synthesizes_missing_hex_and_named_swatches_and_shifts_step_references() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"width": 1080.0, "height": 1080.0, "pages": 1})).unwrap();
        let base = session.doc().unwrap().clone();

        let mut actions = vec![
            EngineAction { command: "frame.create".into(), params: json!({"spread": 0, "rect": [48.0, 80.0, 900.0, 240.0], "content": "text"}) },
            EngineAction { command: "story.setText".into(), params: json!({"story": "$0.story", "text": "قهوة مختصة"}) },
            EngineAction {
                command: "type.char".into(),
                params: json!({"attrs": {"fill": "#1E3A8A", "fontFamily": "NonExistentArabicFontXYZ", "size": 42.0}}),
            },
            // Full-page background emitted AFTER text frame `$0`!
            EngineAction { command: "frame.create".into(), params: json!({"spread": 0, "rect": [0.0, 0.0, 1080.0, 1080.0]}) },
            EngineAction { command: "object.fill".into(), params: json!({"ids": ["$3.id"], "swatch": "WarmCreamSurface"}) },
        ];

        harmonize_actions(&mut actions, &base, &HarmonizeOptions { allow_swatch_synthesis: true, arabic_context: true, ..Default::default() });

        // Two swatches (`Color-1E3A8A` and `WarmCreamSurface`) prepended -> original `$0.story` shifted to `$2.story`!
        assert_eq!(actions[0].command, "swatch.create");
        assert_eq!(actions[1].command, "swatch.create");
        assert_eq!(actions[2].command, "frame.create");
        assert_eq!(actions[3].params["story"], "$2.story");
        assert_eq!(actions[4].params["attrs"]["fill"], "Color-1E3A8A");
        assert_ne!(actions[4].params["attrs"]["fontFamily"], "NonExistentArabicFontXYZ");

        // Full-page background at index 5 must be followed by `object.arrange` sending `$5.id` to `"back"`,
        // and downstream `object.fill` at index 7 must have its `$3.id` shifted to `$5.id`!
        assert_eq!(actions[5].command, "frame.create");
        assert_eq!(actions[6].command, "object.arrange");
        assert_eq!(actions[6].params["ids"], json!(["$5.id"]));
        assert_eq!(actions[6].params["to"], "back");
        assert_eq!(actions[7].command, "object.fill");
        assert_eq!(actions[7].params["ids"], json!(["$5.id"]));
        assert_eq!(actions[7].params["swatch"], "WarmCreamSurface");

        // Verify the harmonized sequence applies cleanly through `production.apply`!
        let cur = session.doc().unwrap();
        session
            .execute(
                "production.apply",
                &json!({
                    "commands": actions,
                    "outputMode": "native_only",
                    "expectedDocumentUid": cur.uid,
                    "expectedRevision": cur.revision,
                    "requirePreflight": false,
                }),
            )
            .unwrap();
        // Background rectangle must now sit at index 0 (behind the text frame) on spread 0!
        let spread = &session.doc().unwrap().doc.spreads[0];
        assert_eq!(spread.items.len(), 2);
        assert!(spread.items[0].text_frame().is_none(), "background rect must be arranged behind text frame");
        assert!(spread.items[1].text_frame().is_some(), "text frame must remain visible in front");
    }

    #[test]
    fn deduplicates_repeated_brand_logo_placements_with_reuse_asset() {
        let dir = tempfile::tempdir().unwrap();
        let logo_path = dir.path().join("logo.png");
        let mut png_bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(64, 64, image::Rgb([30, 58, 138])))
            .write_to(&mut png_bytes, ImageFormat::Png)
            .unwrap();
        std::fs::write(&logo_path, png_bytes.get_ref()).unwrap();

        let palette = extract_brand_palette_from_attachments(&[TaskAttachment {
            path: logo_path.to_string_lossy().into_owned(),
            role: AttachmentRole::BrandLogo,
        }]);
        assert_eq!(palette.len(), 1);
        assert_eq!(palette[0].1, [30, 58, 138]);

        let mut session = Session::new();
        session.execute("file.new", &json!({"width": 1080.0, "height": 1080.0, "pages": 2})).unwrap();
        let base = session.doc().unwrap().clone();
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes.get_ref());

        let mut actions = vec![
            EngineAction {
                command: "file.place".into(),
                params: json!({"spread": 0, "x": 48.0, "y": 48.0, "width": 120.0, "name": "logo.png", "base64": b64}),
            },
            EngineAction {
                command: "file.place".into(),
                params: json!({"spread": 1, "x": 48.0, "y": 48.0, "width": 120.0, "name": "logo.png", "base64": b64}),
            },
        ];

        harmonize_actions(&mut actions, &base, &HarmonizeOptions { allow_swatch_synthesis: false, ..Default::default() });
        assert!(actions[0].params.get("base64").is_some());
        assert!(actions[1].params.get("base64").is_none());
        assert_eq!(actions[1].params["reuseAsset"], "$0.asset");

        let cur = session.doc().unwrap();
        session
            .execute(
                "production.apply",
                &json!({
                    "commands": actions,
                    "outputMode": "native_with_images",
                    "expectedDocumentUid": cur.uid,
                    "expectedRevision": cur.revision,
                    "requirePreflight": false,
                }),
            )
            .unwrap();
        assert_eq!(session.doc().unwrap().doc.assets.len(), 1, "both pages must share 1 deduplicated asset");
    }
}
