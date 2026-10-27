//! Local, measured layout recovery. It never purchases requests or edits the live document.
use designcraft_ai_types::EngineAction;
use designcraft_ai_types::protocol::ProductionPlan;
use designcraft_engine::{
    DocState, Prefs, Session,
    doc::{ItemId, StoryId},
};
use designcraft_geom::{Affine, Rect};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

mod aliases;
mod fit;
mod geometry;
#[cfg(test)]
mod tests;

#[allow(unused_imports)]
use self::aliases::{
    format_populated_new_stories, normalize_alt_fills_aliases, normalize_effects_aliases, normalize_unit_interval_fields, parse_inline_color,
    parse_keyed_channels,
};
pub(crate) use self::aliases::{normalize_command_aliases, recover_unregistered_swatches};
pub(crate) use self::fit::prepare;
#[allow(unused_imports)]
use self::fit::{TextPosition, check_repeated_slots, frame_identity, recover_text_positions};
#[allow(unused_imports)]
use self::geometry::{
    BackgroundGrowth, FittingObstacle, background_growth_blocks, blocks_translation, containing_background_growths, contains, fitting_obstacles,
    is_allowed_fit_location, is_expandable_card_shape, is_snug_growable_background, overlaps, position_collision, shift,
};

pub(crate) fn is_fallback_symbol_or_honorific(ch: char) -> bool {
    if ch.is_whitespace() || ch.is_control() {
        return false;
    }
    matches!(
        ch,
        '©'
            | '®'
            | '°'
            | '±'
            | '²'
            | '³'
            | '¹'
            | '¼'..='¾'
            | '×'
            | '÷'
            | '\u{0600}'..='\u{0605}'
            | '\u{060e}'..='\u{061a}'
            | '\u{06d6}'..='\u{06ed}'
            | '\u{06fd}'..='\u{06fe}'
            | '\u{08d3}'..='\u{08ff}'
            | '\u{2000}'..='\u{2bff}'
            | '\u{2e00}'..='\u{2e7f}'
            | '\u{3000}'..='\u{303f}'
            | '\u{fd3e}'..='\u{fd4f}'
            | '\u{fdcf}'
            | '\u{fdf0}'..='\u{fdff}'
            | '\u{fe30}'..='\u{fe6b}'
            | '\u{fff0}'..='\u{fffd}'
            | '\u{1f000}'..='\u{1faff}'
    )
}

fn is_variation_selector(ch: char) -> bool {
    matches!(ch, '\u{fe00}'..='\u{fe0f}')
}

pub(crate) fn has_fallback_glyph(db: &designcraft_fonts::ScopedFonts<'_>, ch: char) -> bool {
    resolve_fallback_face(db, ch, false, false).is_some()
}

fn resolve_fallback_face(db: &designcraft_fonts::ScopedFonts<'_>, ch: char, bold: bool, italic: bool) -> Option<(String, String)> {
    const PREFERRED: &[&str] = &[
        "Segoe UI Symbol",
        "Segoe UI Historic",
        "Segoe UI Emoji",
        "Traditional Arabic",
        "Sakkal Majalla",
        "Simplified Arabic",
        "Arial",
        "Noto Sans Symbols 2",
        "Noto Sans Symbols",
        "Noto Emoji",
        "Amiri",
        "Noto Naskh Arabic",
        "DejaVu Sans",
        "Apple Symbols",
    ];
    let check_family = |family: &str| -> Option<(String, String)> {
        let styles = db.styles(family);
        if styles.is_empty() {
            return None;
        }
        if let Some(style) = styles.iter().find(|s| {
            let (weight, slanted) = db.traits_of(family, s);
            (weight >= 600.0) == bold && slanted == italic && db.face(family, s).covers(ch)
        }) {
            return Some((family.to_owned(), style.clone()));
        }
        if let Some(style) =
            styles.iter().find(|s| matches!(s.as_str(), "Regular" | "Roman" | "Book" | "Normal" | "Medium") && db.face(family, s).covers(ch))
        {
            return Some((family.to_owned(), style.clone()));
        }
        styles.iter().find(|s| db.face(family, s).covers(ch)).map(|s| (family.to_owned(), s.clone()))
    };
    for &family in PREFERRED {
        if let Some(found) = check_family(family) {
            return Some(found);
        }
    }
    for family in db.families() {
        if let Some(found) = check_family(&family) {
            return Some(found);
        }
    }
    None
}

fn is_combining_arabic_mark(ch: char) -> bool {
    matches!(ch, '\u{0610}'..='\u{061a}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06ed}' | '\u{08d3}'..='\u{08ff}')
}

fn resolve_cluster_fallback_face(db: &designcraft_fonts::ScopedFonts<'_>, cluster: &str, bold: bool, italic: bool) -> Option<(String, String)> {
    const ARABIC_PREFERRED: &[&str] = &[
        "Traditional Arabic",
        "Sakkal Majalla",
        "Simplified Arabic",
        "Amiri",
        "Noto Naskh Arabic",
        "Segoe UI Historic",
        "Arial",
        "Segoe UI Symbol",
        "DejaVu Sans",
    ];
    let covers_all = |family: &str, style: &str| {
        let face = db.face(family, style);
        cluster.chars().filter(|&c| !is_variation_selector(c)).all(|c| face.covers(c))
    };
    let check_family = |family: &str| -> Option<(String, String)> {
        let styles = db.styles(family);
        if styles.is_empty() {
            return None;
        }
        if let Some(style) = styles.iter().find(|s| {
            let (weight, slanted) = db.traits_of(family, s);
            (weight >= 600.0) == bold && slanted == italic && covers_all(family, s)
        }) {
            return Some((family.to_owned(), style.clone()));
        }
        if let Some(style) = styles.iter().find(|s| matches!(s.as_str(), "Regular" | "Roman" | "Book" | "Normal" | "Medium") && covers_all(family, s))
        {
            return Some((family.to_owned(), style.clone()));
        }
        styles.iter().find(|s| covers_all(family, s)).map(|s| (family.to_owned(), s.clone()))
    };
    for &family in ARABIC_PREFERRED {
        if let Some(found) = check_family(family) {
            return Some(found);
        }
    }
    for family in db.families() {
        if let Some(found) = check_family(&family) {
            return Some(found);
        }
    }
    None
}

fn apply_story_glyph_fallbacks(
    story_id: StoryId,
    item_id: ItemId,
    story_ref: &Value,
    scratch: &mut Session,
    candidate: &mut ProductionPlan,
    notes: &mut Vec<Value>,
) -> Result<(), String> {
    let doc = scratch.doc().map_err(|e| e.to_string())?.doc.clone();
    let story = doc.story(story_id).ok_or("Native layout story disappeared")?;
    let db = designcraft_fonts::FontDb::global().scoped(doc.font_scope);
    let mut fallbacks: Vec<(usize, usize, String, String, String)> = Vec::new();
    for (pi, range) in story.para_ranges().into_iter().enumerate() {
        let para = story.paras.get(pi).ok_or("Native paragraph formatting disappeared")?;
        let (_, base_chars) = doc.styles.resolve_para(para);
        let text = story.text.get(range.clone()).ok_or("Invalid native paragraph boundary")?;
        let mut iter = text.char_indices().peekable();
        let mut last_base: Option<(usize, char)> = None;
        while let Some((offset, ch)) = iter.next() {
            let byte = range.start.saturating_add(offset);
            let mut end = byte.saturating_add(ch.len_utf8());
            let combining_mark = is_combining_arabic_mark(ch);
            if !combining_mark && !is_variation_selector(ch) {
                last_base = Some((byte, ch));
            }
            // Only decorative symbols, Arabic honorific ligatures/signs, and missing Arabic
            // combining marks get a fallback. Ordinary copy and Arabic letterforms retain the
            // selected family; a missing requested font remains an explicit error.
            if !is_fallback_symbol_or_honorific(ch) && !combining_mark {
                if is_variation_selector(ch) {
                    let attrs = doc.styles.resolve_char(&base_chars, story.char_format_of(byte));
                    if !db.face(&attrs.font_family, &attrs.font_style).covers(ch) {
                        let (weight, italic) = db.traits_of(&attrs.font_family, &attrs.font_style);
                        if let Some((family, style)) = resolve_fallback_face(&db, ch, weight >= 600.0, italic) {
                            fallbacks.push((byte, end, ch.to_string(), family, style));
                        }
                    }
                }
                continue;
            }
            let attrs = doc.styles.resolve_char(&base_chars, story.char_format_of(byte));
            if db.face(&attrs.font_family, &attrs.font_style).covers(ch) {
                continue;
            }
            let (weight, italic) = db.traits_of(&attrs.font_family, &attrs.font_style);
            let mut span_start = byte;
            let mut cluster_chars = String::new();
            if combining_mark && let Some((base_byte, base_ch)) = last_base {
                span_start = base_byte;
                cluster_chars.push(base_ch);
            }
            cluster_chars.push(ch);
            while let Some(&(next_offset, next_ch)) = iter.peek() {
                if !(is_variation_selector(next_ch) || (combining_mark && is_combining_arabic_mark(next_ch))) {
                    break;
                }
                end = range.start.saturating_add(next_offset).saturating_add(next_ch.len_utf8());
                cluster_chars.push(next_ch);
                let _ = iter.next();
            }
            let resolved = if combining_mark && span_start < byte {
                resolve_cluster_fallback_face(&db, &cluster_chars, weight >= 600.0, italic).or_else(|| {
                    resolve_fallback_face(&db, ch, weight >= 600.0, italic).map(|(f, s)| {
                        span_start = byte;
                        (f, s)
                    })
                })
            } else {
                resolve_fallback_face(&db, ch, weight >= 600.0, italic)
            };
            let (family, style) = resolved.ok_or_else(|| format!("Native text frame {} needs an installed font covering symbol {ch}", item_id.0))?;
            if let Some((_, prev_end, prev_chars, prev_family, prev_style)) = fallbacks.last_mut()
                && *prev_end >= span_start
                && *prev_family == family
                && *prev_style == style
            {
                *prev_end = end.max(*prev_end);
                prev_chars.push_str(&cluster_chars);
            } else {
                fallbacks.push((span_start, end, cluster_chars, family, style));
                if fallbacks.len() > 256 {
                    return Err("Too many symbol fallback ranges in one native story".into());
                }
            }
        }
    }
    for (start, end, symbols, family, style) in fallbacks {
        if candidate.commands.len().checked_add(2).is_none_or(|len| len > 512) {
            return Err("Symbol glyph fallback exceeds the bounded native action limit".into());
        }
        let actions = [
            EngineAction { command: "text.select".into(), params: json!({"story":story_ref,"anchor":start,"focus":end}) },
            EngineAction { command: "type.char".into(), params: json!({"attrs":{"fontFamily":family,"fontStyle":style}}) },
        ];
        scratch.execute("text.select", &json!({"story":story_id.0,"anchor":start,"focus":end})).map_err(|e| e.to_string())?;
        let char_action = actions.get(1).ok_or("Missing symbol fallback formatting action")?;
        scratch.execute("type.char", &char_action.params).map_err(|e| e.to_string())?;
        candidate.commands.extend(actions);
        notes.push(json!({
            "kind":"symbolFontFallback",
            "item":item_id.0,
            "symbol":symbols,
            "fontFamily":family,
            "fontStyle":style,
            "message":"Preserved the character using an installed symbol or honorific fallback font"
        }));
    }
    Ok(())
}
