//! Type › Find/Replace Font: the fonts a document uses (missing ones flagged) and replacing one
//! font with another everywhere — local formatting and style definitions. Type › Reload Fonts:
//! the fonts installed, edited or removed since they were read.

use std::collections::BTreeMap;
use std::sync::Arc;

use designcraft_doc::{CharAttrs, Document, Story};
use designcraft_fonts::FontSource;
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, cmd, has_doc, str_param};
use crate::{Result, Session};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "font.list", "Fonts in Document", [], None, "{} → [{family, style, characters, missing, styleMissing, source: bundled|installed|document|added (null when missing), matchStatus: exact|styleSubstitute|missing, resolvedFamily, resolvedStyle, replacementFamily, replacementStyle}] (missing first)", has_doc, |s, _| {
            Ok(Value::Array(list(&s.doc()?.doc)))
        }),
        cmd!(
            "font.replace",
            "Find/Replace Font…",
            ["Type"],
            None,
            "{family?, style?, toFamily, toStyle?, redefineStyles?: true, script?: arabic|latin, pages?: [start, end]} — replaces the font in text and (by default) in paragraph and character styles",
            has_doc,
            replace
        ),
        cmd!(
            noundo "font.reload",
            "Reload Fonts",
            ["Type"],
            None,
            "{} — scans the font folders again and reads again the installed font files changed or removed since they were read, then recomposes the open documents → {cataloged, changed: [path]}",
            always,
            reload
        ),
    ]
}

/// Fonts installed, edited or removed while the app runs (#328, #547): the font database reads
/// them again, then every open document is composed again with what is installed now.
fn reload(s: &mut Session, _: &Value) -> Result<Value> {
    let reloaded = designcraft_fonts::FontDb::global().reload_fonts();
    // A new cache: render jobs still holding the old one can't bring back the old fonts' layout.
    s.cache = Arc::new(designcraft_compose::Cache::new());
    for st in &mut s.docs {
        st.bump_revision();
    }
    Ok(json!({"cataloged": reloaded.cataloged, "changed": reloaded.changed}))
}

/// Every story's text, including table cells and footnotes.
pub(super) fn for_each_story(d: &Document, f: &mut dyn FnMut(&Story)) {
    let mut pending: Vec<&Story> = d.stories.values().map(AsRef::as_ref).collect();
    while let Some(st) = pending.pop() {
        f(st);
        pending.extend(st.tables.values().flat_map(|t| t.cells.iter().map(|c| &c.text)));
        pending.extend(st.notes.iter().chain(&st.endnotes).map(|n| &n.text));
    }
}

/// Effective document font use shared by the command layer and preflight.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UsedFont {
    pub family: String,
    pub style: String,
    pub characters: usize,
    pub missing: bool,
    pub style_missing: bool,
    pub source: Option<&'static str>,
    pub match_status: &'static str,
    pub resolved_family: String,
    pub resolved_style: String,
    pub replacement_family: String,
    pub replacement_style: String,
}

fn list(d: &Document) -> Vec<Value> {
    used_fonts(d).into_iter().map(|font| json!(font)).collect()
}

pub(super) fn used_fonts(d: &Document) -> Vec<UsedFont> {
    let db = designcraft_fonts::FontDb::global().scoped(d.font_scope);
    let mut used: BTreeMap<(String, String), (usize, std::collections::BTreeSet<char>)> = BTreeMap::new();
    for_each_story(d, &mut |st| {
        let ranges = st.para_ranges();
        for (pi, r) in ranges.iter().enumerate() {
            let (_, base) = d.styles.resolve_para(&st.paras[pi]);
            for (rr, fmt) in st.runs() {
                let n = st.text[rr.start.max(r.start)..rr.end.min(r.end).max(rr.start.max(r.start))].chars().count();
                if n == 0 && !(r.is_empty() && rr.contains(&r.start)) {
                    continue;
                }
                let p = d.styles.resolve_char(&base, fmt);
                if let Some(f) = d.styles.composite_font(&p.font_family) {
                    let text = st.text.get(rr.start.max(r.start)..rr.end.min(r.end).max(rr.start.max(r.start))).unwrap_or("");
                    for c in text.chars() {
                        if let Some(e) = f.entry(c) {
                            let entry = used.entry((e.family.clone(), e.style.clone())).or_default();
                            entry.0 += 1;
                            entry.1.insert(c);
                        }
                    }
                } else {
                    let entry = used.entry((p.font_family, p.font_style)).or_default();
                    entry.0 += n;
                    entry.1.extend(st.text.get(rr.start.max(r.start)..rr.end.min(r.end).max(rr.start.max(r.start))).unwrap_or("").chars());
                }
            }
        }
    });
    let mut out: Vec<UsedFont> = used
        .into_iter()
        .map(|((family, style), (n, characters))| {
            let missing = !db.has_family(&family);
            let style_missing = !missing && !db.has_style(&family, &style);
            let resolved = db.face(&family, &style);
            let characters: Vec<char> = characters.into_iter().filter(|c| !('\u{E000}'..='\u{E1FF}').contains(c)).collect();
            let replacement = db.replacement_face(&family, &style, &characters);
            let source = (!missing).then(|| match db.face(&family, &style).source {
                FontSource::Bundled => "bundled",
                FontSource::Installed(_) => "installed",
                FontSource::Document(_) => "document",
                FontSource::Memory => "added",
            });
            UsedFont {
                family,
                style,
                characters: n,
                missing,
                style_missing,
                source,
                match_status: if missing {
                    "missing"
                } else if style_missing {
                    "styleSubstitute"
                } else {
                    "exact"
                },
                resolved_family: resolved.family.clone(),
                resolved_style: resolved.style.clone(),
                replacement_family: replacement.family.clone(),
                replacement_style: replacement.style.clone(),
            }
        })
        .collect();
    out.sort_by_key(|font| !(font.missing || font.style_missing));
    out
}

/// Replace the font in a sparse attribute set (only where the family is set); true when it changed.
fn swap(a: &mut CharAttrs, family: &str, style: Option<&str>, to_family: &str, to_style: Option<&str>) -> bool {
    let fam_ok = a.font_family.as_deref().is_some_and(|f| f.eq_ignore_ascii_case(family));
    let style_ok = style.is_none_or(|s| a.font_style.as_deref().is_some_and(|x| x.eq_ignore_ascii_case(s)));
    if !fam_ok || !style_ok {
        return false;
    }
    a.font_family = Some(to_family.to_string());
    if let Some(ts) = to_style {
        a.font_style = Some(ts.to_string());
    }
    true
}

fn replace(s: &mut Session, p: &Value) -> Result<Value> {
    let script = str_param(p, "script").map(|v| v.trim().to_ascii_lowercase());
    if let Some(sc) = script.as_deref()
        && !matches!(sc, "arabic" | "latin")
    {
        return Err(bad("font.replace", "`script` must be `arabic` or `latin`"));
    }
    let page_range = super::diacritics::parse_pages_range("font.replace", p, s.doc()?.doc.spreads.iter().map(|sp| sp.pages.len()).sum())?;
    if script.as_deref() == Some("arabic") {
        let family_opt = str_param(p, "family").map(str::to_string);
        let style = str_param(p, "style").map(str::to_string);
        let to_family = str_param(p, "toFamily").ok_or_else(|| bad("font.replace", "missing toFamily"))?.trim().to_string();
        let styles_too = p.get("redefineStyles").and_then(Value::as_bool).unwrap_or(page_range.is_none());
        let db = designcraft_fonts::FontDb::global().scoped(s.doc()?.doc.font_scope);
        let matched = if matches!(to_family.to_ascii_lowercase().as_str(), "auto") || to_family == "تلقائي" {
            db.preferred_arabic_font()
        } else {
            db.resolve_arabic_font(&to_family)
        }
        .ok_or_else(|| bad("font.replace", "Choose an installed replacement Arabic font family with Unicode Arabic glyphs"))?;
        return s.edit(|d, _| {
            let targets = super::diacritics::stories_on_pages(d, page_range, &[]);
            let original_styles = d.styles.clone();
            let scoped_db = designcraft_fonts::FontDb::global().scoped(d.font_scope);

            let mut arabic_used_families = std::collections::BTreeSet::new();
            for_each_story(d, &mut |st| {
                let ranges = st.para_ranges();
                for (pi, r) in ranges.iter().enumerate() {
                    let Some(pf) = st.paras.get(pi) else { continue };
                    let (_, base) = original_styles.resolve_para(pf);
                    for (rr, cf) in st.runs() {
                        let s0 = rr.start.max(r.start);
                        let s1 = rr.end.min(r.end);
                        if s0 >= s1 {
                            continue;
                        }
                        if st.text.get(s0..s1).is_some_and(|slice| slice.chars().any(super::diacritics::is_arabic_letter)) {
                            let eff = original_styles.resolve_char(&base, cf);
                            arabic_used_families.insert(eff.font_family.to_ascii_lowercase());
                        }
                    }
                }
            });

            let matches_style_family = |fam: &str, st_opt: Option<&str>| -> bool {
                if let Some(sf) = style.as_deref()
                    && st_opt.is_none_or(|x| !x.eq_ignore_ascii_case(sf))
                {
                    return false;
                }
                if let Some(ff) = family_opt.as_deref()
                    && ff != "*"
                {
                    return fam.eq_ignore_ascii_case(ff);
                }
                arabic_used_families.contains(&fam.to_ascii_lowercase()) || scoped_db.resolve_arabic_font(fam).is_some()
            };

            let mut redefined_para_styles = std::collections::BTreeSet::new();
            let mut redefined_char_styles = std::collections::BTreeSet::new();
            if styles_too {
                for ps in &original_styles.paragraph {
                    if ps.chars.font_family.as_deref().is_some_and(|f| matches_style_family(f, ps.chars.font_style.as_deref())) {
                        redefined_para_styles.insert(ps.name.clone());
                    }
                }
                // Propagate to derived styles that inherit `font_family` from a redefined base style.
                for _ in 0..original_styles.paragraph.len() {
                    let mut grew = false;
                    for ps in &original_styles.paragraph {
                        if ps.chars.font_family.is_none()
                            && ps.based_on.as_deref().is_some_and(|b| redefined_para_styles.contains(b))
                            && redefined_para_styles.insert(ps.name.clone())
                        {
                            grew = true;
                        }
                    }
                    if !grew {
                        break;
                    }
                }
                for cs in &original_styles.character {
                    if cs.chars.font_family.as_deref().is_some_and(|f| matches_style_family(f, cs.chars.font_style.as_deref())) {
                        redefined_char_styles.insert(cs.name.clone());
                    }
                }
                for _ in 0..original_styles.character.len() {
                    let mut grew = false;
                    for cs in &original_styles.character {
                        if cs.chars.font_family.is_none()
                            && cs.based_on.as_deref().is_some_and(|b| redefined_char_styles.contains(b))
                            && redefined_char_styles.insert(cs.name.clone())
                        {
                            grew = true;
                        }
                    }
                    if !grew {
                        break;
                    }
                }
            }

            let mut changed = 0usize;
            for sid in targets {
                if let Some(st) = d.story_mut(sid) {
                    changed = changed.saturating_add(super::diacritics::replace_arabic_in_story(
                        st,
                        &original_styles,
                        &scoped_db,
                        &matched,
                        family_opt.as_deref(),
                        style.as_deref(),
                        &redefined_para_styles,
                        &redefined_char_styles,
                    ));
                }
            }

            if styles_too {
                let st = d.styles_mut();
                for ps in &mut st.paragraph {
                    if let Some(cur_fam) = ps.chars.font_family.as_deref()
                        && matches_style_family(cur_fam, ps.chars.font_style.as_deref())
                    {
                        let cur_st = ps.chars.font_style.as_deref().unwrap_or("Regular");
                        let (w, _) = scoped_db.traits_of(cur_fam, cur_st);
                        let bold = super::diacritics::style_looks_bold(cur_st, w);
                        let (nf, ns) =
                            if bold { (&matched.bold_family, &matched.bold_style) } else { (&matched.regular_family, &matched.regular_style) };
                        if !cur_fam.eq_ignore_ascii_case(nf) || !cur_st.eq_ignore_ascii_case(ns) {
                            ps.chars.font_family = Some(nf.clone());
                            ps.chars.font_style = Some(ns.clone());
                            changed = changed.saturating_add(1);
                        }
                    }
                }
                for cs in &mut st.character {
                    if let Some(cur_fam) = cs.chars.font_family.as_deref()
                        && matches_style_family(cur_fam, cs.chars.font_style.as_deref())
                    {
                        let cur_st = cs.chars.font_style.as_deref().unwrap_or("Regular");
                        let (w, _) = scoped_db.traits_of(cur_fam, cur_st);
                        let bold = super::diacritics::style_looks_bold(cur_st, w);
                        let (nf, ns) =
                            if bold { (&matched.bold_family, &matched.bold_style) } else { (&matched.regular_family, &matched.regular_style) };
                        if !cur_fam.eq_ignore_ascii_case(nf) || !cur_st.eq_ignore_ascii_case(ns) {
                            cs.chars.font_family = Some(nf.clone());
                            cs.chars.font_style = Some(ns.clone());
                            changed = changed.saturating_add(1);
                        }
                    }
                }
                for f in &mut st.composite_fonts {
                    for e in &mut f.entries {
                        if matches_style_family(&e.family, Some(&e.style)) {
                            let (w, _) = scoped_db.traits_of(&e.family, &e.style);
                            let bold = super::diacritics::style_looks_bold(&e.style, w);
                            let (nf, ns) =
                                if bold { (&matched.bold_family, &matched.bold_style) } else { (&matched.regular_family, &matched.regular_style) };
                            if !e.family.eq_ignore_ascii_case(nf) || !e.style.eq_ignore_ascii_case(ns) {
                                e.family.clone_from(nf);
                                e.style.clone_from(ns);
                                changed = changed.saturating_add(1);
                            }
                        }
                    }
                }
            }
            Ok(json!({"changed": changed, "resolvedFamily": matched.family}))
        });
    }
    let family = str_param(p, "family").ok_or_else(|| bad("font.replace", "missing family"))?.to_string();
    let style = str_param(p, "style").map(str::to_string);
    let to_family = str_param(p, "toFamily").ok_or_else(|| bad("font.replace", "missing toFamily"))?.to_string();
    let to_style = str_param(p, "toStyle").map(str::to_string);
    let db = designcraft_fonts::FontDb::global().scoped(s.doc()?.doc.font_scope);
    if family.trim().is_empty() || to_family.trim().is_empty() || !db.has_family(&to_family) {
        return Err(bad("font.replace", "Choose an available replacement font family"));
    }
    if let Some(style) = &to_style
        && !db.has_style(&to_family, style)
    {
        return Err(bad("font.replace", "Choose an available replacement font style"));
    }
    let resolved = db.face(&to_family, to_style.as_deref().or(style.as_deref()).unwrap_or("Regular"));
    if resolved.is_placeholder() {
        return Err(bad("font.replace", "A placeholder font cannot replace document text"));
    }
    let to_family = resolved.family.clone();
    let to_style = (to_style.is_some() || style.is_some()).then(|| resolved.style.clone());
    let styles_too = p.get("redefineStyles").and_then(Value::as_bool).unwrap_or(true);
    s.edit(|d, _| {
        let mut changed = 0usize;
        // Resolve text against the original styles before redefining them. Font family and
        // style can be inherited independently through paragraph and character formatting.
        let original_styles = d.styles.clone();
        let ids: Vec<_> = super::diacritics::stories_on_pages(d, page_range, &[]).into_iter().collect();
        for sid in ids {
            let Some(st) = d.story_mut(sid) else { continue };
            let mut pending = vec![st];
            while let Some(st) = pending.pop() {
                let mut ranges = Vec::new();
                for (pi, paragraph) in st.para_ranges().iter().enumerate() {
                    let Some(format) = st.paras.get(pi) else { continue };
                    let (_, base) = original_styles.resolve_para(format);
                    for (run, format) in st.runs() {
                        let start = run.start.max(paragraph.start);
                        // An empty paragraph still uses its paragraph-break character's font.
                        let end = if paragraph.is_empty() && run.contains(&paragraph.start) {
                            st.text.get(start..).and_then(|tail| tail.chars().next()).map_or(start, |c| start.saturating_add(c.len_utf8()))
                        } else {
                            run.end.min(paragraph.end)
                        };
                        if start >= end {
                            continue;
                        }
                        let effective = original_styles.resolve_char(&base, format);
                        if effective.font_family.eq_ignore_ascii_case(&family)
                            && style.as_ref().is_none_or(|s| effective.font_style.eq_ignore_ascii_case(s))
                        {
                            ranges.push((start..end, db.face(&to_family, to_style.as_deref().unwrap_or(&effective.font_style)).style.clone()));
                        }
                    }
                }
                for (range, replacement_style) in ranges {
                    st.format_chars(range, |format| {
                        format.over.font_family = Some(to_family.clone());
                        format.over.font_style = Some(replacement_style.clone());
                    });
                    changed += 1;
                }
                pending.extend(st.tables.values_mut().flat_map(|t| Arc::make_mut(t).cells.iter_mut().map(|c| &mut c.text)));
                pending.extend(st.notes.iter_mut().chain(&mut st.endnotes).map(|n| &mut Arc::make_mut(n).text));
            }
        }
        if styles_too {
            let st = d.styles_mut();
            for ps in &mut st.paragraph {
                changed += swap(&mut ps.chars, &family, style.as_deref(), &to_family, to_style.as_deref()) as usize;
            }
            for cs in &mut st.character {
                changed += swap(&mut cs.chars, &family, style.as_deref(), &to_family, to_style.as_deref()) as usize;
            }
            for f in &mut st.composite_fonts {
                for e in &mut f.entries {
                    if e.family.eq_ignore_ascii_case(&family) && style.as_ref().is_none_or(|s| e.style.eq_ignore_ascii_case(s)) {
                        e.family.clone_from(&to_family);
                        e.style = db.face(&to_family, to_style.as_deref().unwrap_or(&e.style)).style.clone();
                        changed += 1;
                    }
                }
            }
        }
        Ok(json!({"changed": changed}))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_fonts_recomposes_open_documents_without_editing_them() {
        let mut s = Session::new();
        let r = s.execute("font.reload", &json!({})).unwrap();
        assert!(r["cataloged"].is_u64() && r["changed"].is_array(), "{r}");
        s.execute("file.new", &json!({})).unwrap();
        s.execute("frame.create", &json!({"rect": [72, 72, 400, 300], "content": "text", "text": "Some text here."})).unwrap();
        let (revision, undo) = (s.doc().unwrap().revision, s.doc().unwrap().history.undo.len());
        let cache = Arc::clone(&s.cache);
        s.execute("font.reload", &json!({})).unwrap();
        assert!(!Arc::ptr_eq(&cache, &s.cache), "the layout composed with the old fonts is let go");
        let st = s.doc().unwrap();
        assert_ne!(st.revision, revision, "the canvas draws the document again");
        assert_eq!(st.history.undo.len(), undo, "reloading fonts isn't an edit");
    }

    #[test]
    fn replacement_resolves_inherited_family_and_style_and_rejects_missing_targets() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("frame.create", &json!({"rect": [0,0,200,200], "content":"text", "text":"Inherited\n\nEnd"})).unwrap();
        let d = Arc::make_mut(&mut s.doc_mut().unwrap().doc);
        let sid = *d.stories.keys().next().unwrap();
        let story = d.story_mut(sid).unwrap();
        for para in &mut story.paras {
            para.chars.font_family = Some("Unavailable Family".into());
        }
        let before = s.execute("font.list", &json!({})).unwrap();
        let style = before[0]["style"].clone();
        assert!(s.execute("font.replace", &json!({"family":"Unavailable Family", "toFamily":"Also Missing"})).is_err());
        assert_eq!(before, s.execute("font.list", &json!({})).unwrap());
        s.execute("font.replace", &json!({"family":"Unavailable Family", "style":style, "toFamily":"Source Sans 3", "toStyle":"Regular"})).unwrap();
        assert!(s.execute("font.list", &json!({})).unwrap().as_array().unwrap().iter().all(|f| f["missing"] == false));
        s.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(before, s.execute("font.list", &json!({})).unwrap());
    }

    #[test]
    fn find_and_replace_font() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let r = s.execute("frame.create", &json!({"rect": [72, 72, 400, 300], "content": "text", "text": "Some text here."})).unwrap();
        let sid = s.doc().unwrap().doc.item(designcraft_doc::ItemId(r["id"].as_u64().unwrap())).unwrap().text_frame().unwrap().story;
        s.execute("text.select", &json!({"story": sid.0, "anchor": 0, "focus": 4})).unwrap();
        s.execute("type.char", &json!({"fontFamily": "Nonexistent Sans", "fontStyle": "Bold"})).unwrap();
        let l = s.execute("font.list", &json!({})).unwrap();
        assert_eq!(l[0]["family"], "Nonexistent Sans", "missing fonts first: {l}");
        assert_eq!(l[0]["missing"], true);
        assert_eq!(l[0]["characters"], 4);
        // Replace it with an available family.
        let avail = l.as_array().unwrap().iter().find(|f| f["missing"] == false).unwrap()["family"].as_str().unwrap().to_string();
        let r = s.execute("font.replace", &json!({"family": "Nonexistent Sans", "toFamily": avail, "toStyle": "Regular"})).unwrap();
        assert_eq!(r["changed"], 1);
        let l = s.execute("font.list", &json!({})).unwrap();
        assert!(l.as_array().unwrap().iter().all(|f| f["missing"] == false), "{l}");
        // Style definitions are redefined too.
        s.execute("style.paragraph.create", &json!({"name": "Odd", "chars": {"fontFamily": "Nonexistent Sans"}})).unwrap();
        let r = s.execute("font.replace", &json!({"family": "Nonexistent Sans", "toFamily": avail})).unwrap();
        assert_eq!(r["changed"], 1);
        assert_eq!(s.doc().unwrap().doc.styles.para("Odd").unwrap().chars.font_family.as_deref(), Some(avail.as_str()));
    }

    /// InDesign names a family installed in several formats `Family (OTF)`; the plain family
    /// stands for it, and a family that isn't available under either name is still missing.
    #[test]
    fn a_family_with_a_format_suffix_uses_the_plain_family() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let r = s.execute("frame.create", &json!({"rect": [72, 72, 400, 300], "content": "text", "text": "Some text here."})).unwrap();
        let sid = s.doc().unwrap().doc.item(designcraft_doc::ItemId(r["id"].as_u64().unwrap())).unwrap().text_frame().unwrap().story;
        s.execute("text.select", &json!({"story": sid.0, "anchor": 0, "focus": 15})).unwrap();
        s.execute("type.char", &json!({"fontFamily": "Source Serif 4 (OTF)", "fontStyle": "Bold"})).unwrap();
        let l = s.execute("font.list", &json!({})).unwrap();
        let entry = l.as_array().unwrap().iter().find(|f| f["family"] == "Source Serif 4 (OTF)").unwrap();
        assert_eq!((&entry["missing"], &entry["styleMissing"], &entry["source"]), (&json!(false), &json!(false), &json!("bundled")), "{l}");
        let st = s.doc().unwrap();
        let cs = s.cache.get(&st.doc, sid, None);
        let faces: Vec<_> =
            cs.frames.iter().flat_map(|f| &f.lines).flat_map(|l| &l.glyphs).filter(|g| g.visible && g.len > 0).map(|g| g.face.get()).collect();
        assert!(!faces.is_empty() && faces.iter().all(|f| f.family == "Source Serif 4" && f.style == "Bold"));
        let kinds = |s: &mut Session| {
            s.execute("preflight.run", &json!({})).unwrap()["issues"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["kind"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert!(!kinds(&mut s).iter().any(|k| k == "missingFont"));

        s.execute("text.select", &json!({"story": sid.0, "anchor": 0, "focus": 4})).unwrap();
        s.execute("type.char", &json!({"fontFamily": "Nope (OTF)"})).unwrap();
        let l = s.execute("font.list", &json!({})).unwrap();
        assert_eq!((&l[0]["family"], &l[0]["missing"]), (&json!("Nope (OTF)"), &json!(true)), "{l}");
        assert!(kinds(&mut s).iter().any(|k| k == "missingFont"));
    }

    /// A style name that differs only in spacing (Montilla Design's file sets `Regular ` with a
    /// trailing space) is the face the text is drawn in, not a missing style.
    #[test]
    fn a_style_with_extra_spacing_is_not_missing() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let r = s.execute("frame.create", &json!({"rect": [72, 72, 400, 300], "content": "text", "text": "Text"})).unwrap();
        let sid = s.doc().unwrap().doc.item(designcraft_doc::ItemId(r["id"].as_u64().unwrap())).unwrap().text_frame().unwrap().story;
        s.execute("text.select", &json!({"story": sid.0, "anchor": 0, "focus": 4})).unwrap();
        s.execute("type.char", &json!({"fontFamily": "Source Serif 4", "fontStyle": "Regular "})).unwrap();
        let l = s.execute("font.list", &json!({})).unwrap();
        let entry = l.as_array().unwrap().iter().find(|f| f["style"] == "Regular ").unwrap();
        assert_eq!((&entry["missing"], &entry["styleMissing"]), (&json!(false), &json!(false)), "{l}");
        s.execute("type.char", &json!({"fontFamily": "Source Serif 4", "fontStyle": "Nope"})).unwrap();
        let l = s.execute("font.list", &json!({})).unwrap();
        assert_eq!(l[0]["styleMissing"], json!(true), "{l}");
    }
}

#[cfg(test)]
mod composite_tests {
    use super::*;
    use designcraft_doc::{
        ParaFormat,
        cjk::{CompositeFont, CompositeFontEntry},
    };

    #[test]
    fn inventory_resolves_composites_in_nested_cell_notes() {
        use designcraft_doc::{StoryId, Table, build::NewDocument};
        let mut d = Document::new(&NewDocument::default());
        d.styles_mut().composite_fonts.push(CompositeFont {
            name: "Mixed".into(),
            entries: vec![
                CompositeFontEntry { family: "Source Serif 4".into(), ..Default::default() },
                CompositeFontEntry { characters: "1".into(), family: "Source Sans 3".into(), ..Default::default() },
            ],
        });
        let mut story = Story::new(StoryId(d.alloc()));
        let mut table = Table::new(1, 1, 1, 0, 0, 100.0);
        let cell = &mut table.cells[0].text;
        cell.insert_note(0, "1", ParaFormat::default());
        Arc::make_mut(&mut cell.notes[0]).text.format_chars(0..1, |f| f.over.font_family = Some("Mixed".into()));
        story.insert_table(0, table);
        d.stories.insert(story.id, Arc::new(story));
        let fonts = list(&d);
        assert!(fonts.iter().any(|f| f["family"] == "Source Sans 3" && f["characters"] == 1));
        assert!(!fonts.iter().any(|f| f["family"] == "Mixed"));
    }

    #[test]
    fn preflight_and_font_list_follow_used_composite_entries_in_notes() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("frame.create", &json!({"rect": [20,20,400,600], "content":"text", "text":"Body"})).unwrap();
        let d = Arc::make_mut(&mut s.doc_mut().unwrap().doc);
        d.styles_mut().composite_fonts.push(CompositeFont {
            name: "Mixed".into(),
            entries: vec![
                CompositeFontEntry { family: "Source Serif 4".into(), ..Default::default() },
                CompositeFontEntry { characters: "1".into(), family: "Missing Note Digits".into(), ..Default::default() },
                CompositeFontEntry { characters: "2".into(), family: "Unused Missing Digits".into(), ..Default::default() },
            ],
        });
        let sid = *d.stories.keys().next().unwrap();
        let st = d.story_mut(sid).unwrap();
        st.insert_note(4, "A1", ParaFormat::default());
        Arc::make_mut(&mut st.notes[0]).text.format_chars(0..2, |f| f.over.font_family = Some("CompositeFont/Mixed".into()));
        let list = s.execute("font.list", &json!({})).unwrap();
        assert!(list.as_array().unwrap().iter().any(|f| f["family"] == "Missing Note Digits" && f["characters"] == 1));
        assert!(!list.to_string().contains("Unused Missing Digits"));
        let issues = s.execute("preflight.run", &json!({})).unwrap();
        let fonts: Vec<_> = issues["issues"].as_array().unwrap().iter().filter(|i| i["kind"] == "missingFont").collect();
        assert_eq!(fonts.len(), 1, "{issues}");
        assert!(fonts[0]["message"].as_str().unwrap().contains("Missing Note Digits"));
        s.execute("font.replace", &json!({"family":"Missing Note Digits", "toFamily":"Source Sans 3", "toStyle":"Regular"})).unwrap();
        let issues = s.execute("preflight.run", &json!({})).unwrap();
        assert!(issues["issues"].as_array().unwrap().iter().all(|i| i["kind"] != "missingFont"));
        assert_eq!(s.doc().unwrap().doc.styles.composite_fonts[0].entries[1].characters, "1");
        s.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(s.doc().unwrap().doc.styles.composite_fonts[0].entries[1].family, "Missing Note Digits");
    }
}

#[cfg(test)]
mod resolution_tests {
    use super::*;

    #[test]
    fn inventory_distinguishes_missing_styles_and_replacement_chooses_an_available_style() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("frame.create", &json!({"rect":[20,20,300,400],"content":"text","text":"Body"})).unwrap();
        let d = Arc::make_mut(&mut s.doc_mut().unwrap().doc);
        let sid = *d.stories.keys().next().unwrap();
        d.story_mut(sid).unwrap().format_chars(0..4, |f| {
            f.over.font_family = Some("Source Sans 3".into());
            f.over.font_style = Some("Unavailable Style".into());
        });
        let before = s.execute("font.list", &json!({})).unwrap();
        assert_eq!(before[0]["matchStatus"], "styleSubstitute");
        assert_eq!(before[0]["missing"], false);
        assert_eq!(before[0]["styleMissing"], true);
        assert_eq!(before[0]["resolvedStyle"], "Regular");
        let preflight = s.execute("preflight.run", &json!({})).unwrap();
        assert!(preflight["issues"].as_array().unwrap().iter().any(|i| i["kind"] == "missingFontStyle"));
        s.execute("font.replace", &json!({"family":"Source Sans 3","toFamily":"Source Serif 4"})).unwrap();
        let after = s.execute("font.list", &json!({})).unwrap();
        assert_eq!(after[0]["matchStatus"], "exact");
        s.execute("edit.undo", &json!({})).unwrap();
        assert_eq!(before, s.execute("font.list", &json!({})).unwrap());
    }

    #[test]
    fn arabic_script_replacement_redefines_styles_while_preserving_latin_runs() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let db = designcraft_fonts::FontDb::global().scoped(s.doc().unwrap().doc.font_scope);
        let Some(ar) = db.preferred_arabic_font() else {
            return;
        };
        s.execute(
            "style.paragraph.create",
            &json!({"name": "Lesson Body", "chars": {"fontFamily": "Source Sans 3", "fontStyle": "Regular", "size": 13}}),
        )
        .unwrap();
        let mixed = "مرحبا بكم في Lesson One مع الصلاة على النبي ﷺ";
        let f = s.execute("frame.create", &json!({"rect": [36, 36, 420, 180], "content": "text", "text": mixed})).unwrap();
        let sid = designcraft_doc::StoryId(f["story"].as_u64().unwrap());
        let len = s.doc().unwrap().doc.stories[&sid].len();
        s.execute("text.select", &json!({"story": sid.0, "anchor": 0, "focus": len})).unwrap();
        s.execute("style.paragraph.apply", &json!({"name": "Lesson Body", "clearOverrides": true})).unwrap();

        let res = s.execute("font.replace", &json!({"script": "arabic", "toFamily": "auto", "redefineStyles": true})).unwrap();
        assert_eq!(res["resolvedFamily"], ar.family);

        let d = &s.doc().unwrap().doc;
        let ps = d.styles.para("Lesson Body").unwrap();
        assert_eq!(ps.chars.font_family.as_deref(), Some(ar.regular_family.as_str()));

        let st = d.story(sid).unwrap();
        let (_, base) = d.styles.resolve_para(&st.paras[0]);
        let latin_pos = st.text.find("Lesson").unwrap();
        let latin_cf = st.char_format_at(latin_pos + 1);
        let latin_eff = d.styles.resolve_char(&base, latin_cf);
        assert_eq!(latin_eff.font_family, "Source Sans 3", "Latin sub-run preserved its Latin font");

        let arabic_eff = d.styles.resolve_char(&base, st.char_format_at(2));
        assert_eq!(arabic_eff.font_family, ar.regular_family, "Arabic sub-run uses resolved Arabic family");
    }
}
