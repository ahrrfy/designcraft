//! Bounded user-authored page scripts and deterministic native math activities.
use crate::worker::BatchScope;
use designcraft_ai_types::EngineAction;
use designcraft_ai_types::protocol::{ImagePlacement, MAX_BRIEF_BYTES, ProductionPlan};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const MAX_PAGES: u32 = 1000;
const MAX_ACTIONS: usize = 512;
const MAX_IMAGE_PROMPT: usize = 8192;
const MAX_COPY_BINDINGS: usize = 256;
const MAX_COPY_WORDS: usize = 65_536;
const COPY_LABEL: &str = "designcraft.copy.v1:";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CopyBinding {
    frame: usize,
    copy_id: String,
    word_range: [usize; 2],
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CopyProvenance {
    brief_sha256: String,
    copy_id: String,
    word_range: [usize; 2],
}

struct CopySource {
    page: u32,
    language: &'static str,
    segment: usize,
    text: String,
    words: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct ScriptPage {
    pub page: u32,
    pub title: String,
    pub english_print_copy: Option<String>,
    pub arabic_print_copy: Option<String>,
    pub illustration_prompt: Option<String>,
    pub full_script: String,
}

#[derive(Clone, Debug)]
pub(crate) struct ScriptContract {
    pub global_instructions: String,
    pub pages: Vec<ScriptPage>,
}

fn end(scope: BatchScope) -> Result<u32, String> {
    scope.start.checked_add(scope.count).filter(|end| scope.count > 0 && *end <= MAX_PAGES).ok_or_else(|| "Invalid explicit-script page scope".into())
}

fn script_digit_value(c: char) -> Option<u32> {
    match c {
        '0'..='9' => Some(u32::from(c as u8 - b'0')),
        '\u{0660}'..='\u{0669}' => Some(u32::from(c) - 0x0660),
        '\u{06F0}'..='\u{06F9}' => Some(u32::from(c) - 0x06F0),
        _ => None,
    }
}

fn is_arabic_combining_mark(c: char) -> bool {
    ('\u{064B}'..='\u{065F}').contains(&c) || c == '\u{0670}' || ('\u{06D6}'..='\u{06ED}').contains(&c)
}

fn base_char_count(s: &str) -> usize {
    s.chars().filter(|&c| !is_arabic_combining_mark(c)).count()
}

fn has_explicit_script_markers(brief: &str) -> bool {
    let mut bare_arabic_page_lines = 0usize;
    for line in brief.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("* **") || trimmed.starts_with("- **") {
            return true;
        }
        for prefix in ["صفحة", "الصفحة"] {
            if let Some(rest) = trimmed.strip_prefix(prefix)
                && rest.starts_with(char::is_whitespace)
                && rest.trim_start().starts_with(|c| script_digit_value(c).is_some())
                && rest.contains(':')
            {
                bare_arabic_page_lines = bare_arabic_page_lines.saturating_add(1);
                if bare_arabic_page_lines >= 2 {
                    return true;
                }
            }
        }
    }
    false
}

fn page_heading(line: &str, allow_bare_arabic: bool) -> Result<Option<(u32, String)>, String> {
    let line = line.trim();
    let (heading, is_markdown) = if line.starts_with('#') {
        (line.trim_start_matches('#').trim_start(), true)
    } else if allow_bare_arabic {
        (line, false)
    } else {
        return Ok(None);
    };
    let (rest, is_arabic_kw) = if is_markdown && heading.get(..4).is_some_and(|prefix| prefix.eq_ignore_ascii_case("page")) {
        (heading.get(4..).ok_or("Invalid page heading boundary")?, false)
    } else if let Some(rest) = heading.strip_prefix("الصفحة").or_else(|| heading.strip_prefix("صفحة")) {
        (rest, true)
    } else {
        return Ok(None);
    };
    if !rest.starts_with(char::is_whitespace) {
        return Ok(None);
    }
    let rest = rest.trim_start();
    let boundary = rest.find(|c: char| script_digit_value(c).is_none()).unwrap_or(rest.len());
    let digits_str = rest.get(..boundary).ok_or("Invalid page number boundary")?;
    if digits_str.is_empty() {
        return if is_markdown { Err("Explicit PAGE headings require a number from 1 to 1000".into()) } else { Ok(None) };
    }
    let mut number = 0u32;
    for ch in digits_str.chars() {
        let d = script_digit_value(ch).ok_or("Explicit PAGE headings require a number from 1 to 1000")?;
        number = number.checked_mul(10).and_then(|n| n.checked_add(d)).ok_or("Explicit PAGE headings require a number from 1 to 1000")?;
    }
    if !(1..=MAX_PAGES).contains(&number) {
        return Err("Explicit PAGE headings require a number from 1 to 1000".into());
    }
    let suffix = rest.get(boundary..).ok_or("Invalid page title boundary")?.trim_start();
    if is_arabic_kw && is_markdown && suffix.is_empty() {
        return Ok(Some((number - 1, format!("الصفحة {number}"))));
    }
    let stripped =
        suffix.strip_prefix('—').or_else(|| suffix.strip_prefix('–')).or_else(|| suffix.strip_prefix('-')).or_else(|| suffix.strip_prefix(':'));
    let Some(raw_title) = stripped else {
        return if is_markdown { Err("Explicit PAGE headings require a dash or colon before the title".into()) } else { Ok(None) };
    };
    let title = raw_title.trim();
    if title.is_empty() || base_char_count(title) > 512 || title.chars().count() > 1536 {
        return Err("Explicit page title is empty or exceeds 512 characters".into());
    }
    Ok(Some((number - 1, title.to_owned())))
}

fn field_heading(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    let rest = line.strip_prefix("* **").or_else(|| line.strip_prefix("- **"))?;
    let (label, value) = rest.split_once("**")?;
    Some((label.trim_end_matches(':').trim(), value.trim_start_matches(':').trim()))
}

/// Equal, explicit percentage bands are a user layout request, not an inferred
/// agent template. Missing, ambiguous or different layouts retain novelty checks.
pub(crate) fn authored_layout_signatures(brief: &str) -> Result<BTreeMap<u64, String>, String> {
    let Some(contract) = parse(brief)? else { return Ok(BTreeMap::new()) };
    Ok(contract.pages.iter().filter_map(|page| layout_signature(&page.full_script).map(|signature| (u64::from(page.page), signature))).collect())
}

fn layout_signature(script: &str) -> Option<String> {
    let mut layout = None;
    let mut active = false;
    let mut fenced = false;
    for line in script.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if let Some((label, value)) = field_heading(line) {
            let label = label.to_ascii_lowercase();
            active = label == "layout & grid architecture" || label == "layout architecture";
            if active {
                if layout.is_some() {
                    return None;
                }
                layout = Some(value.to_owned());
            }
        } else if active && let Some(value) = &mut layout {
            value.push(' ');
            value.push_str(line.trim());
        }
    }
    let layout = layout?;
    let mut parts = layout.split('%').peekable();
    let mut zones = BTreeMap::new();
    let mut total = 0u16;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            break;
        }
        let part = part.trim_end();
        let prefix = part.trim_end_matches(|c: char| c.is_ascii_digit());
        if prefix.ends_with(['.', ',']) {
            return None;
        }
        let percent = part.get(prefix.len()..)?.parse::<u8>().ok().filter(|n| (1..=100).contains(n))?;
        let name = prefix
            .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '(' | ':' | '*' | '`'))
            .rsplit(|c: char| !c.is_alphabetic())
            .next()?
            .to_ascii_lowercase();
        let region = match name.as_str() {
            "top" => "top",
            "middle" | "central" | "center" | "centre" => "middle",
            "bottom" => "bottom",
            "left" => "left",
            "right" => "right",
            "header" => "header",
            "footer" => "footer",
            "body" | "main" => "body",
            _ => return None,
        };
        if zones.insert(region, percent).is_some() || zones.len() > 8 {
            return None;
        }
        total = total.checked_add(u16::from(percent))?;
    }
    if zones.len() < 2 || total != 100 {
        return None;
    }
    Some(zones.into_iter().map(|(name, percent)| format!("{name}:{percent}")).collect::<Vec<_>>().join(";"))
}

fn page_fields(body: &str) -> Result<(Option<String>, Option<String>, Option<String>), String> {
    let mut fields: [Option<String>; 3] = [None, None, None];
    let mut active = None;
    for line in body.lines() {
        let value = if let Some((label, value)) = field_heading(line) {
            active = if label.starts_with("English Print Copy") || label.starts_with("النص الإنجليزي") {
                Some(0)
            } else if label.starts_with("Arabic Print Copy") || label.starts_with("النص العربي") || label.starts_with("النص المطبوع")
            {
                Some(1)
            } else if label.contains("Illustration Prompt") || label.contains("وصف الرسم") || label.contains("التوجيه البصري") {
                Some(2)
            } else {
                None
            };
            if let Some(index) = active {
                let field = fields.get_mut(index).ok_or("Invalid script field")?;
                if field.is_some() {
                    return Err("Explicit page repeats a print-copy or illustration field".into());
                }
                *field = Some(String::new());
            }
            value
        } else {
            line.trim().strip_prefix('>').map(str::trim_start).unwrap_or(line.trim())
        };
        if let Some(index) = active {
            let field = fields.get_mut(index).and_then(Option::as_mut).ok_or("Missing script field")?;
            if !field.is_empty() && !value.is_empty() {
                field.push('\n');
            }
            field.push_str(value);
        }
    }
    for field in fields.iter_mut().flatten() {
        *field = field.trim().to_owned();
        if field.is_empty() {
            return Err("Explicit page has an empty designated print-copy or illustration field".into());
        }
    }
    let [english, arabic, illustration] = fields;
    Ok((english, arabic, illustration))
}

/// Parse only explicit Markdown PAGE blocks. A generic brief keeps its existing path.
pub(crate) fn parse(brief: &str) -> Result<Option<ScriptContract>, String> {
    if brief.len() > MAX_BRIEF_BYTES || brief.contains('\0') {
        return Err("Page script exceeds the shared brief limit or contains a null character".into());
    }
    let allow_bare = has_explicit_script_markers(brief);
    let mut headers = Vec::new();
    let mut offset = 0usize;
    let mut fenced = false;
    for line in brief.split_inclusive('\n') {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
        } else if !fenced && let Some((page, title)) = page_heading(line, allow_bare)? {
            let expected = u32::try_from(headers.len()).map_err(|_| "Too many explicit page headings")?;
            if page != expected {
                return Err(format!("Explicit page script must contain each page once in order; expected PAGE {:02}", u64::from(expected) + 1));
            }
            if headers.len() >= usize::try_from(MAX_PAGES).map_err(|_| "Invalid page limit")? {
                return Err("Explicit page script exceeds 1000 pages".into());
            }
            headers.push((offset, offset.checked_add(line.len()).ok_or("Script offset overflow")?, page, title));
        }
        offset = offset.checked_add(line.len()).ok_or("Script offset overflow")?;
    }
    let Some((first, _, _, _)) = headers.first() else {
        return Ok(None);
    };
    let global_instructions = brief.get(..*first).ok_or("Invalid global script boundary")?.trim().to_owned();
    let mut pages = Vec::with_capacity(headers.len());
    for (index, (start, body_start, page, title)) in headers.iter().enumerate() {
        let stop = headers.get(index.saturating_add(1)).map(|header| header.0).unwrap_or(brief.len());
        let body = brief.get(*body_start..stop).ok_or("Invalid explicit page script boundary")?;
        let (english_print_copy, arabic_print_copy, illustration_prompt) = page_fields(body)?;
        pages.push(ScriptPage {
            page: *page,
            title: title.clone(),
            english_print_copy,
            arabic_print_copy,
            illustration_prompt,
            full_script: brief.get(*start..stop).ok_or("Invalid explicit page block")?.trim().to_owned(),
        });
    }
    Ok(Some(ScriptContract { global_instructions, pages }))
}

impl ScriptContract {
    fn scoped_pages(&self, scope: BatchScope) -> Result<&[ScriptPage], String> {
        let end = end(scope)?;
        let range = usize::try_from(scope.start).map_err(|_| "Invalid explicit page start")?
            ..usize::try_from(end).map_err(|_| "Invalid explicit page end")?;
        self.pages.get(range).ok_or_else(|| "Requested pages are missing from the explicit user page script".into())
    }

    pub(crate) fn context(&self, scope: BatchScope, total_pages: u32) -> Result<Value, String> {
        if u64::try_from(self.pages.len()).map_err(|_| "Too many explicit pages")? != u64::from(total_pages) {
            return Err("Explicit page-script count must match the requested document page count".into());
        }
        Ok(json!({"outline_status":"explicit_user_page_script","page_numbering":"page is zero-based; PAGE headings are one-based",
            "global_production_specifications":self.global_instructions,"scoped_page_plan":self.scoped_pages(scope)?,
            "precedence_rule":"Follow these exact authored page topics, sequence, layout requirements and designated printable copy. Do not insert a fallback contents page, cycle families, change lessons, remove diacritics, or replace supplied copy. Split copy across native text frames as needed. Icon/name-line markers describe native objects; retain numeral practice arrays. Native math activities must bind their stated quantities; raster semantic correctness still requires human visual review."}))
    }
}

/// Keep shared prepress/style directions while avoiding repeated unrelated page blocks.
pub(crate) fn scoped_prompt(brief: &str, scope: BatchScope) -> Result<String, String> {
    let Some(contract) = parse(brief)? else {
        return Ok(brief.to_owned());
    };
    let mut output = contract.global_instructions.clone();
    for page in contract.scoped_pages(scope)? {
        output.push_str("\n\n");
        output.push_str(&page.full_script);
    }
    Ok(output)
}

fn facing_link_hint(pages: &[&ScriptPage]) -> (bool, &'static str) {
    let (Some(first), Some(second)) = (pages.first(), pages.get(1)) else {
        return (false, "standalone_page");
    };
    if pages.iter().any(|page| {
        let source = page.full_script.to_lowercase();
        ["two-page spread", "double-page spread", "across both pages", "صفحتين متقابلتين", "صفحتان متقابلتان"]
            .iter()
            .any(|phrase| source.contains(phrase))
    }) {
        return (true, "authored_facing_page_direction");
    }
    let first_title = crate::template_plan::normalize_curriculum_query(&first.title);
    let second_title = crate::template_plan::normalize_curriculum_query(&second.title);
    if [&first_title, &second_title].iter().any(|title| {
        (title.contains("unit") && title.contains("opener")) || title.contains("افتتاح") || (title.contains("الوحدة") && title.contains("مقدمة"))
    }) {
        return (true, "unit_opener");
    }
    let meaningful = |word: &str| {
        word.chars().count() >= 4
            && !matches!(
                word,
                "page"
                    | "unit"
                    | "lesson"
                    | "independent"
                    | "practice"
                    | "activity"
                    | "activities"
                    | "number"
                    | "numbers"
                    | "count"
                    | "counting"
                    | "math"
                    | "mathematics"
                    | "with"
                    | "from"
                    | "this"
                    | "that"
                    | "book"
                    | "trace"
                    | "write"
                    | "review"
                    | "صفحة"
                    | "الوحدة"
                    | "الدرس"
                    | "تدريب"
                    | "تدريبات"
                    | "النشاط"
                    | "التمرين"
                    | "تمارين"
                    | "الكتاب"
                    | "الرياضيات"
                    | "الاعداد"
                    | "العدد"
            )
    };
    let first_topics: BTreeSet<_> = first_title.split(|c: char| !c.is_alphanumeric()).filter(|word| meaningful(word)).collect();
    if second_title.split(|c: char| !c.is_alphanumeric()).any(|word| meaningful(word) && first_topics.contains(word)) {
        (true, "shared_authored_title_topic")
    } else {
        (false, "independent_lessons")
    }
}

/// Facing context never widens the production/review scope. Future companion
/// scripts are public design input, not permission to generate or approve them.
/// Pair IDs use absolute human page numbers, independent of chunk boundaries.
pub(crate) fn facing_context(brief: &str, scope: BatchScope) -> Result<Value, String> {
    let end = end(scope)?;
    let produce_only_pages: Vec<_> = (scope.start..end).collect();
    let mut context = json!({"status":"no_explicit_page_script","scope":scope,"produce_only_pages":produce_only_pages,"pairs":[],
        "page_numbering":"page and produce_only_pages are zero-based; human_pages are one-based",
        "physical_pairing":"Cover page 1 stands alone, followed by absolute facing pairs 2–3, 4–5, 6–7 and so on. The document binding determines physical left/right placement.",
        "text_direction":{"arabic":"rightToLeft","english":"leftToRight"},
        "coordination_policy":"Retain the book's common header, palette, recurring characters and illustration identity. Occasionally connect a facing pair when the authored unit opener or related activity benefits from it; linked_candidate is an advisory cue, never a requirement to link every pair. Keep each page's designated copy and individual lesson complete.",
        "scope_policy":"Produce commands and image placements only for produce_only_pages. Companion members marked context_only may guide shared motifs, headers, palettes and related activities; do not generate, replace, approve or advance their pages. Context grants no provider or human approval.",
        "art_policy":"Use original supplied or permitted artwork; do not copy publisher art. Matching scene directions do not guarantee identical generated pixels; human visual proof remains required."});
    let Some(contract) = parse(brief)? else {
        return Ok(context);
    };
    contract.scoped_pages(scope)?;
    let starts: BTreeSet<_> = (scope.start..end).map(|page| if page == 0 || page % 2 == 1 { page } else { page.saturating_sub(1) }).collect();
    let mut pairs = Vec::with_capacity(starts.len());
    for first in starts {
        let nominal_last = if first == 0 { 0 } else { first.checked_add(1).ok_or("Facing companion page overflow")? };
        let mut sources = Vec::with_capacity(2);
        for page in first..=nominal_last {
            if let Some(source) = contract.pages.get(usize::try_from(page).map_err(|_| "Invalid facing companion index")?) {
                sources.push(source);
            }
        }
        let human_first = first.checked_add(1).ok_or("Facing page number overflow")?;
        let human_last = nominal_last.checked_add(1).ok_or("Facing companion number overflow")?;
        let pair_id = if first == 0 { "cover-0001".to_owned() } else { format!("pages-{human_first:04}-{human_last:04}") };
        let (linked_candidate, link_reason) = if first == 0 { (false, "standalone_cover") } else { facing_link_hint(&sources) };
        let members: Vec<_> = sources
            .iter()
            .map(|page| {
                json!({"page":page.page,"human_page":u64::from(page.page) + 1,
            "role":if (scope.start..end).contains(&page.page) {"produce"} else {"context_only"},
            "title":page.title,"illustration_prompt":page.illustration_prompt,"full_script":page.full_script})
            })
            .collect();
        let human_pages: Vec<_> = sources.iter().map(|page| u64::from(page.page) + 1).collect();
        pairs.push(json!({"pair_id":pair_id,"human_pages":human_pages,"members":members,
            "linked_candidate":linked_candidate,"link_reason":link_reason,"link_status":"advisory_source_context_only"}));
    }
    context["status"] = json!("explicit_script_facing_context");
    context["pairs"] = json!(pairs);
    Ok(context)
}

fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn contains_copy(actual: &str, expected: &str) -> bool {
    let word = |c: char| {
        c.is_alphanumeric()
            || c == '_'
            || matches!(c, '\u{0300}'..='\u{036f}' | '\u{0610}'..='\u{061a}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06ed}')
    };
    let starts_word = expected.chars().next().is_some_and(word);
    let ends_word = expected.chars().next_back().is_some_and(word);
    actual.match_indices(expected).any(|(offset, _)| {
        let before = actual.get(..offset).and_then(|text| text.chars().next_back());
        let after = offset.checked_add(expected.len()).and_then(|end| actual.get(end..)).and_then(|text| text.chars().next());
        (!starts_word || before.is_none_or(|c| !word(c))) && (!ends_word || after.is_none_or(|c| !word(c)))
    })
}

fn printable_segment(segment: &str) -> String {
    let mut remaining = segment;
    let mut result = String::with_capacity(segment.len());
    while let Some((before, after)) = remaining.split_once('[') {
        result.push_str(before);
        let Some((marker, tail)) = after.split_once(']') else {
            result.push('[');
            result.push_str(after);
            return normalized(&result);
        };
        if !(marker.starts_with("ICON:") || marker.starts_with("أيقونة:") || matches!(marker, "Name Line" | "Line" | "سطر الاسم" | "سطر"))
        {
            result.push('[');
            result.push_str(marker);
            result.push(']');
        }
        remaining = tail;
    }
    result.push_str(remaining);
    normalized(&result)
}

fn reference_index(value: Option<&Value>) -> Result<usize, String> {
    let reference = value.and_then(Value::as_str).ok_or("Script-copy validation needs a native story reference")?;
    let body = reference
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .or_else(|| reference.strip_prefix('$'))
        .ok_or("Invalid script-copy story reference")?;
    let (index, path) = body.split_once('.').ok_or("Invalid script-copy story reference")?;
    if path != "story" {
        return Err("Script-copy mutation must target a native frame story".into());
    }
    index.parse().map_err(|_| "Invalid script-copy story index".into())
}

fn native_stories(plan: &ProductionPlan) -> Result<BTreeMap<usize, (u32, String)>, String> {
    let mut stories: BTreeMap<usize, (u32, String)> = BTreeMap::new();
    for (index, action) in plan.commands.iter().enumerate() {
        if action.command == "frame.create" && action.params.get("content").and_then(Value::as_str) == Some("text") {
            let page = action
                .params
                .get("spread")
                .and_then(Value::as_u64)
                .and_then(|page| u32::try_from(page).ok())
                .ok_or("Explicit-script text frame needs a page")?;
            let text = action.params.get("text").and_then(Value::as_str).unwrap_or("");
            stories.insert(index, (page, text.replace("$$", "$")));
        } else if action.command == "story.setText" {
            let source = reference_index(action.params.get("story"))?;
            let story = stories.get_mut(&source).ok_or("Script-copy mutation targets an unknown story")?;
            story.1 = action.params.get("text").and_then(Value::as_str).ok_or("Script-copy mutation needs text")?.replace("$$", "$");
        } else if matches!(action.command.as_str(), "text.insert" | "table.insert" | "table.setCell") {
            return Err(
                "Explicit-script copy must use final native frame text or story.setText so every supplied word can be verified before images".into(),
            );
        }
    }
    Ok(stories)
}

fn native_copy(plan: &ProductionPlan) -> Result<BTreeMap<u32, String>, String> {
    let stories = native_stories(plan)?;
    let mut pages: BTreeMap<u32, String> = BTreeMap::new();
    for (page, text) in stories.into_values() {
        let copy = pages.entry(page).or_default();
        if copy.len().saturating_add(text.len()).saturating_add(1) > MAX_BRIEF_BYTES {
            return Err("Native page copy exceeds its bounded verification limit".into());
        }
        copy.push(' ');
        copy.push_str(&text);
    }
    Ok(pages.into_iter().map(|(page, text)| (page, normalized(&text))).collect())
}

/// This proves literal native copy coverage, not raster objects or reading order.
pub(crate) fn validate_plan(brief: &str, scope: BatchScope, plan: &ProductionPlan) -> Result<(), String> {
    let Some(contract) = parse(brief)? else {
        return Ok(());
    };
    let bound = bound_copy_ids(brief, scope, plan)?;
    let copy = native_copy(plan)?;
    for page in contract.scoped_pages(scope)? {
        let actual = copy
            .get(&page.page)
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| format!("Page {} has no verifiable native printable copy", u64::from(page.page) + 1))?;
        let mut designated = false;
        for (language, code, expected) in [("English", "en", &page.english_print_copy), ("Arabic", "ar", &page.arabic_print_copy)] {
            if let Some(expected) = expected {
                designated = true;
                for (index, raw) in expected.split('|').enumerate() {
                    let segment = printable_segment(raw);
                    if segment.is_empty() || bound.contains(&copy_id(page.page, code, index)?) {
                        continue;
                    }
                    if !contains_copy(actual, &segment) {
                        let preview: String = segment.chars().take(100).collect();
                        return Err(format!("Page {} is missing or changes supplied {language} print copy: {preview}", u64::from(page.page) + 1));
                    }
                }
            }
        }
        if !designated && !contains_copy(actual, &normalized(&page.title)) {
            return Err(format!("Page {} does not preserve its explicit authored title", u64::from(page.page) + 1));
        }
    }
    Ok(())
}

/// Ground each purchased image in its own authored lesson. Native preflight does
/// not establish whether a raster contains the right animals or exact counts.
pub(crate) fn anchor_images(brief: &str, scope: BatchScope, plan: &mut ProductionPlan) -> Result<(), String> {
    let Some(contract) = parse(brief)? else {
        return Ok(());
    };
    let pages = contract.scoped_pages(scope)?;
    let mut prompts = Vec::with_capacity(plan.images.len());
    for image in &plan.images {
        let page = pages
            .iter()
            .find(|page| u64::from(page.page) == u64::try_from(image.page).unwrap_or(u64::MAX))
            .ok_or("Illustration is outside its explicit page contract")?;
        let art = page.illustration_prompt.as_deref().unwrap_or("");
        let prompt = if image.instance_count() > 1 {
            format!(
                "USER PAGE {} — {}\nNATIVE REPEATED COUNT UNIT (1 OF {} NATIVE FRAMES):\nPAGE CONTEXT (FOR SUBJECT IDENTITY AND ART STYLE ONLY — DO NOT RENDER THE TOTAL GROUP COUNT IN THIS SINGLE FRAME):\n{art}\nSINGLE-UNIT SUBJECT TO RENDER:\n{}\nRender EXACTLY ONE single isolated subject matching the page's object identity and style on a clean plain background with generous margin. Do NOT draw multiple copies or groups in this image. The host repeats the single unit in {} native frames to form the exact quantity. Include no text, captions, letters, numerals, borders or speech bubbles. Human visual proof remains required.",
                u64::from(page.page) + 1,
                page.title,
                image.instance_count(),
                image.prompt,
                image.instance_count()
            )
        } else {
            format!(
                "USER PAGE {} — {}\nSUPPLIED SCENE AND SUBJECT REQUIREMENTS:\n{art}\nSELECTED ILLUSTRATION DETAIL:\n{}\nRender only the requested illustrative scene, objects and characters for this page. Preserve the supplied entities and requested quantities. All words, numerals, diagrams, number frames, tracing lines, labels and page layout in the source directions are handled as editable native objects: include none of them in raster artwork. No text, captions, letters, numerals or speech bubbles. This prompt does not certify the generated pixels; human visual proof remains required.",
                u64::from(page.page) + 1,
                page.title,
                image.prompt
            )
        };
        if prompt.len() > MAX_IMAGE_PROMPT {
            return Err("Page-anchored illustration prompt exceeds 8192 bytes; split the illustration into focused scene slots".into());
        }
        prompts.push(prompt);
    }
    for (image, prompt) in plan.images.iter_mut().zip(prompts) {
        image.prompt = prompt;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CountFrame {
    Five,
    Ten,
    Twenty,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeActivity {
    pub page: u32,
    pub rect: [f64; 4],
    pub frame: CountFrame,
    pub quantity: u8,
    pub filled: bool,
}

pub(crate) fn activity_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["page","rect","frame","quantity","filled"],"properties":{
        "page":{"type":"integer","minimum":0,"maximum":999},
        "rect":{"type":"array","minItems":4,"maxItems":4,"items":{"type":"number","minimum":0,"maximum":100000}},
        "frame":{"type":"string","enum":["five","ten","twenty"]},"quantity":{"type":"integer","minimum":0,"maximum":20},
        "filled":{"type":"boolean","description":"true for filled counters; false for outlined circles to color"}
    }})
}

fn activity_shape(commands: &mut Vec<EngineAction>, base: usize, page: u32, rect: [f64; 4], shape: &str, label: String) -> Result<String, String> {
    let index = base.checked_add(commands.len()).ok_or("Native activity reference overflow")?;
    if index.checked_add(2).is_none_or(|end| end > MAX_ACTIONS) {
        return Err("Native count-frame activities exceed the 512-command plan limit; split the internal page chunk".into());
    }
    commands.push(EngineAction { command: "frame.create".into(), params: json!({"spread":page,"rect":rect,"content":"unassigned","shape":shape}) });
    commands.push(EngineAction { command: "object.label".into(), params: json!({"ids":[format!("${index}.id")],"label":label}) });
    Ok(format!("${index}.id"))
}

/// Build mathematical counters locally; a five-frame has 1×5 cells, a
/// ten-frame has 2×5, and a twenty-frame has 4×5, filled left-to-right across the top row first.
/// The caller still validates page geometry and human-reviewed pedagogy.
pub(crate) fn append_activities(plan: &mut ProductionPlan, scope: BatchScope, activities: &[NativeActivity]) -> Result<(), String> {
    let end = end(scope)?;
    if activities.len() > 16 {
        return Err("At most 16 bounded native activities are allowed per internal plan".into());
    }
    let mut commands = Vec::new();
    for (activity_index, activity) in activities.iter().enumerate() {
        let rows = match activity.frame {
            CountFrame::Five => 1u8,
            CountFrame::Ten => 2u8,
            CountFrame::Twenty => 4u8,
        };
        let capacity = rows.saturating_mul(5);
        let [left, top, right, bottom] = activity.rect;
        if activity.page < scope.start
            || activity.page >= end
            || activity.quantity > capacity
            || activity.rect.iter().any(|value| !value.is_finite() || !(0.0..=100_000.0).contains(value))
            || right - left < 25.0
            || bottom - top < f64::from(rows) * 5.0
        {
            return Err("Native count frame has an invalid page, rectangle or quantity for its five/ten/twenty cells".into());
        }
        for earlier in activities.get(..activity_index).ok_or("Invalid native activity range")? {
            let [a, b, c, d] = earlier.rect;
            if earlier.page == activity.page && left < c && right > a && top < d && bottom > b {
                return Err("Native count-frame activity rectangles overlap".into());
            }
        }
        let width = (right - left) / 5.0;
        let height = (bottom - top) / f64::from(rows);
        let frame = match activity.frame {
            CountFrame::Five => "five",
            CountFrame::Ten => "ten",
            CountFrame::Twenty => "twenty",
        };
        let mut all = Vec::new();
        let mut outlined = Vec::new();
        let mut filled = Vec::new();
        for cell in 0..capacity {
            let x = left + f64::from(cell % 5) * width;
            let y = top + f64::from(cell / 5) * height;
            let cell_id = activity_shape(
                &mut commands,
                plan.commands.len(),
                activity.page,
                [x, y, x + width, y + height],
                "rectangle",
                format!("activity.count_frame.{activity_index}.{frame}.cell.{cell}"),
            )?;
            all.push(cell_id.clone());
            outlined.push(cell_id);
            if cell < activity.quantity {
                let radius = width.min(height) * 0.24;
                let center_x = x + width / 2.0;
                let center_y = y + height / 2.0;
                let token = activity_shape(
                    &mut commands,
                    plan.commands.len(),
                    activity.page,
                    [center_x - radius, center_y - radius, center_x + radius, center_y + radius],
                    "ellipse",
                    format!("activity.count_frame.{activity_index}.{frame}.token.{cell}"),
                )?;
                all.push(token.clone());
                if activity.filled { filled.push(token) } else { outlined.push(token) }
            }
        }
        for (ids, swatch) in [(&outlined, "[None]"), (&filled, "[Black]")] {
            if !ids.is_empty() {
                commands.push(EngineAction { command: "object.fill".into(), params: json!({"ids":ids,"swatch":swatch}) });
            }
        }
        commands.push(EngineAction { command: "object.stroke".into(), params: json!({"ids":all,"swatch":"[Black]","weight":0.8}) });
        if plan.commands.len().checked_add(commands.len()).is_none_or(|n| n > MAX_ACTIONS) {
            return Err("Native activity formatting exceeds the 512-command plan limit; simplify this page without changing review size".into());
        }
    }
    plan.commands.extend(commands);
    Ok(())
}

/// Backward-compatible plan envelope. Activities become engine commands before
/// any image purchase; they never enter an immutable style manifest.
pub(crate) fn compile_native_plan(text: &str, scope: BatchScope) -> Result<ProductionPlan, String> {
    compile_native_content(text, scope, None)
}

pub(crate) fn compile_native_content(text: &str, scope: BatchScope, brief: Option<&str>) -> Result<ProductionPlan, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Content {
        title: String,
        commands: Vec<EngineAction>,
        #[serde(default)]
        images: Vec<ImagePlacement>,
        #[serde(default)]
        activities: Vec<NativeActivity>,
        #[serde(default)]
        copy_bindings: Vec<CopyBinding>,
    }
    if text.len() > MAX_BRIEF_BYTES {
        return Err("Native content plan exceeds 1 MiB".into());
    }
    let mut de = serde_json::Deserializer::from_str(text);
    let content: Content = serde_path_to_error::deserialize(&mut de).map_err(|error| format!("Invalid native content plan: {error}"))?;
    de.end().map_err(|error| format!("Invalid native content plan: {error}"))?;
    if content.commands.len() > MAX_ACTIONS {
        return Err("Native content plan exceeds 512 commands".into());
    }
    if content.commands.iter().any(|action| {
        action.command == "object.label"
            && action.params.get("label").and_then(Value::as_str).is_some_and(|label| label.starts_with("designcraft.copy."))
    }) {
        return Err("Provider commands cannot supply reserved host copy provenance labels".into());
    }
    let mut plan = ProductionPlan { title: content.title, commands: content.commands, images: content.images };
    crate::native_layout::normalize_command_aliases(&mut plan);
    normalize_command_references(&mut plan.commands);
    crate::image_repetition::normalize(&mut plan)?;
    if !content.copy_bindings.is_empty() {
        bind_copy(&mut plan, scope, brief.ok_or("Copy bindings require the original explicit page brief")?, &content.copy_bindings)?;
    }
    append_activities(&mut plan, scope, &content.activities)?;
    Ok(plan)
}

fn produces_result_field(action: &EngineAction, field: &str) -> bool {
    match field {
        "id" => matches!(
            action.command.as_str(),
            "frame.create"
                | "line.create"
                | "path.create"
                | "object.group"
                | "file.place"
                | "object.qrCode"
                | "object.pathfinder"
                | "object.makeCompoundPath"
        ),
        "story" => {
            (action.command == "frame.create" && action.params.get("content").and_then(Value::as_str) == Some("text"))
                || matches!(action.command.as_str(), "table.insert" | "text.thread" | "text.unthread")
        }
        "ids" => matches!(action.command.as_str(), "transform.move" | "edit.stepAndRepeat" | "object.releaseCompoundPath"),
        "from" | "to" => action.command == "text.thread",
        "asset" => action.command == "file.place",
        "table" => action.command == "table.insert",
        "content" => action.command == "object.qrCode",
        "swatch" | "kind" | "stops" => action.command == "object.gradient",
        "name" => matches!(
            action.command.as_str(),
            "swatch.create"
                | "style.paragraph.create"
                | "style.character.create"
                | "style.object.create"
                | "style.table.create"
                | "style.cell.create"
        ),
        _ => false,
    }
}

fn is_non_text_creation(action: &EngineAction) -> bool {
    matches!(action.command.as_str(), "line.create" | "path.create")
        || (action.command == "object.qrCode" && action.params.get("rect").is_some())
        || (action.command == "frame.create" && action.params.get("content").and_then(Value::as_str) != Some("text"))
}

fn rewrite_reference_token(token: &str, earlier: &[EngineAction], prefer_non_text_next: bool) -> Option<String> {
    let (braced, body) = if let Some(inner) = token.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        (true, inner)
    } else if let Some(inner) = token.strip_prefix('$') {
        (false, inner)
    } else {
        return None;
    };
    let (step_str, field) = body.split_once('.')?;
    if !matches!(field, "id" | "story" | "table") {
        return None;
    }
    let step = step_str.parse::<usize>().ok()?;
    let source = earlier.get(step)?;
    if produces_result_field(source, field) {
        return None;
    }
    let resolved =
        if field == "id" && prefer_non_text_next && step.checked_add(1).and_then(|next| earlier.get(next)).is_some_and(is_non_text_creation) {
            step.checked_add(1)
        } else if field == "story"
            && is_non_text_creation(source)
            && let Some(paired) = step
                .checked_add(1)
                .into_iter()
                .flat_map(|start| start..earlier.len())
                .take_while(|&candidate| !earlier.get(candidate).is_some_and(is_non_text_creation))
                .find(|&candidate| earlier.get(candidate).is_some_and(|act| produces_result_field(act, "story")))
        {
            Some(paired)
        } else {
            (0..=step).rev().find(|&candidate| earlier.get(candidate).is_some_and(|act| produces_result_field(act, field))).or_else(|| {
                step.checked_add(1)
                    .into_iter()
                    .flat_map(|start| start..earlier.len())
                    .find(|&candidate| earlier.get(candidate).is_some_and(|act| produces_result_field(act, field)))
            })
        }?;
    Some(if braced { format!("${{{resolved}.{field}}}") } else { format!("${resolved}.{field}") })
}

fn normalize_command_references(commands: &mut [EngineAction]) {
    for index in 0..commands.len() {
        let (earlier, rest) = commands.split_at_mut(index);
        let Some((action, _)) = rest.split_first_mut() else {
            continue;
        };
        let prefer_non_text_next = matches!(action.command.as_str(), "object.stroke" | "object.fill" | "object.fittingOptions");
        let cmd_id = action.command.clone();
        let Some(params) = action.params.as_object_mut() else {
            continue;
        };
        let keys: &[&str] = match cmd_id.as_str() {
            "text.thread" => &["ids", "frame", "story", "table", "from", "to"],
            "object.matchAttributes" => &["ids", "frame", "story", "table", "from"],
            _ => &["ids", "frame", "story", "table"],
        };
        for key in keys {
            match params.get_mut(*key) {
                Some(Value::String(reference)) => {
                    if let Some(rewritten) = rewrite_reference_token(reference, earlier, prefer_non_text_next) {
                        *reference = rewritten;
                    }
                }
                Some(Value::Array(items)) => {
                    for item in items.iter_mut() {
                        if let Value::String(reference) = item
                            && let Some(rewritten) = rewrite_reference_token(reference, earlier, prefer_non_text_next)
                        {
                            *reference = rewritten;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn copy_id(page: u32, language: &str, index: usize) -> Result<String, String> {
    Ok(format!("p{}.{}.{}", u64::from(page) + 1, language, index.checked_add(1).ok_or("Copy segment index overflow")?))
}

fn copy_sources(brief: &str, scope: BatchScope) -> Result<BTreeMap<String, CopySource>, String> {
    end(scope)?;
    let Some(contract) = parse(brief)? else {
        return Ok(BTreeMap::new());
    };
    let mut sources = BTreeMap::new();
    let mut word_count = 0usize;
    for page in contract.scoped_pages(scope)? {
        for (language, field) in [("en", &page.english_print_copy), ("ar", &page.arabic_print_copy)] {
            if let Some(field) = field {
                for (segment, raw) in field.split('|').enumerate() {
                    let text = printable_segment(raw);
                    if text.is_empty() {
                        continue;
                    }
                    let mut words = Vec::new();
                    for word in text.split_whitespace() {
                        word_count =
                            word_count.checked_add(1).filter(|count| *count <= MAX_COPY_WORDS).ok_or("Source copy exceeds 65536 bounded words")?;
                        words.push(word.to_owned());
                    }
                    sources.insert(copy_id(page.page, language, segment)?, CopySource { page: page.page, language, segment, text, words });
                }
            }
        }
    }
    Ok(sources)
}

fn copy_words(source: &CopySource, range: [usize; 2]) -> Result<String, String> {
    let [start, end] = range;
    if start >= end || end > MAX_COPY_WORDS {
        return Err("Copy binding word range must be nonempty and within the source word limit".into());
    }
    source.words.get(start..end).map(|words| words.join(" ")).ok_or_else(|| "Copy binding word range exceeds its original source segment".into())
}

fn complete_copy_ranges(coverage: &mut BTreeMap<String, Vec<[usize; 2]>>, sources: &BTreeMap<String, CopySource>) -> Result<(), String> {
    for (id, ranges) in coverage {
        ranges.sort_unstable();
        let mut cursor = 0usize;
        for &[start, end] in ranges.iter() {
            if start != cursor || end <= start {
                return Err(format!("Copy binding {id} has missing or duplicate source-word coverage"));
            }
            cursor = end;
        }
        if cursor != sources.get(id).ok_or("Copy binding source is not in the current page scope")?.words.len() {
            return Err(format!("Copy binding {id} must cover its complete original segment"));
        }
    }
    Ok(())
}

fn bind_copy(plan: &mut ProductionPlan, scope: BatchScope, brief: &str, bindings: &[CopyBinding]) -> Result<(), String> {
    if bindings.len() > MAX_COPY_BINDINGS || plan.commands.len().checked_add(bindings.len()).is_none_or(|count| count > MAX_ACTIONS) {
        return Err("Copy bindings exceed 256 entries or the complete 512-command plan limit".into());
    }
    let sources = copy_sources(brief, scope)?;
    let digest = format!("{:x}", Sha256::digest(brief.as_bytes()));
    let mut frames = BTreeSet::new();
    let mut coverage: BTreeMap<String, Vec<[usize; 2]>> = BTreeMap::new();
    let mut assignments = Vec::with_capacity(bindings.len());
    let mut id_counts = BTreeMap::<&str, usize>::new();
    for binding in bindings {
        *id_counts.entry(binding.copy_id.as_str()).or_default() += 1;
    }
    let explicit_frames: BTreeSet<usize> = bindings
        .iter()
        .map(|binding| binding.frame)
        .filter(|&frame| {
            plan.commands
                .get(frame)
                .is_some_and(|action| action.command == "frame.create" && action.params.get("content").and_then(Value::as_str) == Some("text"))
        })
        .collect();
    for binding in bindings {
        if binding.copy_id.is_empty() || binding.copy_id.len() > 64 {
            return Err("Copy binding has an invalid source ID or repeats a native frame".into());
        }
        let source = sources.get(&binding.copy_id).ok_or("Copy binding source is not in the current explicit page scope")?;
        let candidate = plan.commands.get(binding.frame).ok_or("Copy binding frame is outside the authored command plan")?;
        let resolved_frame = if candidate.command == "frame.create" && candidate.params.get("content").and_then(Value::as_str) == Some("text") {
            binding.frame
        } else {
            (0..=binding.frame)
                .find(|&idx| {
                    !frames.contains(&idx)
                        && !explicit_frames.contains(&idx)
                        && plan.commands.get(idx).is_some_and(|action| {
                            action.command == "frame.create"
                                && action.params.get("content").and_then(Value::as_str) == Some("text")
                                && action.params.get("spread").and_then(Value::as_u64) == Some(u64::from(source.page))
                        })
                })
                .unwrap_or(binding.frame)
        };
        if !frames.insert(resolved_frame) {
            return Err("Copy binding has an invalid source ID or repeats a native frame".into());
        }
        let frame = plan.commands.get(resolved_frame).ok_or("Copy binding frame is outside the authored command plan")?;
        if frame.command != "frame.create"
            || frame.params.get("content").and_then(Value::as_str) != Some("text")
            || frame.params.get("spread").and_then(Value::as_u64) != Some(u64::from(source.page))
        {
            return Err("Copy binding must target a native text frame on its exact source page".into());
        }
        let word_range = if id_counts.get(binding.copy_id.as_str()) == Some(&1)
            && binding.word_range[0] == 0
            && (1..=MAX_COPY_WORDS).contains(&binding.word_range[1])
            && !source.words.is_empty()
        {
            [0, source.words.len()]
        } else {
            binding.word_range
        };
        let text = copy_words(source, word_range)?;
        coverage.entry(binding.copy_id.clone()).or_default().push(word_range);
        let provenance = CopyProvenance { brief_sha256: digest.clone(), copy_id: binding.copy_id.clone(), word_range };
        let label = format!("{COPY_LABEL}{}", serde_json::to_string(&provenance).map_err(|error| error.to_string())?);
        assignments.push((resolved_frame, text, label));
    }
    complete_copy_ranges(&mut coverage, &sources)?;
    for (frame, text, label) in assignments {
        plan.commands
            .get_mut(frame)
            .and_then(|action| action.params.as_object_mut())
            .ok_or("Copy binding frame parameters must be an object")?
            .insert("text".into(), json!(text.replace('$', "$$")));
        plan.commands.push(EngineAction { command: "object.label".into(), params: json!({"ids":[format!("${frame}.id")],"label":label}) });
    }
    // A later story.setText must not undo a host-bound fragment before caching.
    bound_copy_ids(brief, scope, plan)?;
    Ok(())
}

fn label_frame(action: &EngineAction, index: usize) -> Result<usize, String> {
    let ids = action
        .params
        .get("ids")
        .and_then(Value::as_array)
        .filter(|ids| ids.len() == 1)
        .ok_or("Copy provenance must label exactly one native frame")?;
    let reference = ids.first().and_then(Value::as_str).ok_or("Copy provenance requires an earlier native frame reference")?;
    direct_frame(reference, index)
}

fn direct_frame(reference: &str, index: usize) -> Result<usize, String> {
    let body = reference
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .or_else(|| reference.strip_prefix('$'))
        .ok_or("Invalid copy provenance frame reference")?;
    let (frame, path) = body.split_once('.').ok_or("Invalid copy provenance frame reference")?;
    let frame = frame.parse::<usize>().map_err(|_| "Invalid copy provenance frame index")?;
    if path != "id" || frame >= index {
        return Err("Copy provenance must reference an earlier direct native frame ID".into());
    }
    Ok(frame)
}

fn bound_copy_ids(brief: &str, scope: BatchScope, plan: &ProductionPlan) -> Result<BTreeSet<String>, String> {
    let mut provenance = Vec::new();
    for (index, action) in plan.commands.iter().enumerate().filter(|(_, action)| action.command == "object.label") {
        if let Some(label) = action.params.get("label").and_then(Value::as_str).filter(|label| label.starts_with("designcraft.copy.")) {
            let body = label.strip_prefix(COPY_LABEL).filter(|body| body.len() <= 2048).ok_or("Unknown or oversized host copy provenance label")?;
            let value: CopyProvenance = serde_json::from_str(body).map_err(|error| format!("Invalid host copy provenance: {error}"))?;
            if provenance.len() >= MAX_COPY_BINDINGS {
                return Err("Cached copy provenance exceeds 256 bindings".into());
            }
            provenance.push((index, label_frame(action, index)?, label, value));
        }
    }
    if provenance.is_empty() {
        return Ok(BTreeSet::new());
    }
    let sources = copy_sources(brief, scope)?;
    let stories = native_stories(plan)?;
    let digest = format!("{:x}", Sha256::digest(brief.as_bytes()));
    let mut frames = BTreeSet::new();
    let mut coverage: BTreeMap<String, Vec<[usize; 2]>> = BTreeMap::new();
    for (label_index, frame, label, value) in provenance {
        if value.brief_sha256 != digest || value.copy_id.len() > 64 || !frames.insert(frame) {
            return Err("Host copy provenance is stale, forged or repeated for a native frame".into());
        }
        let source = sources.get(&value.copy_id).ok_or("Host copy provenance source is outside the current page scope")?;
        let (page, text) = stories.get(&frame).ok_or("Host copy provenance does not reference a native text story")?;
        if *page != source.page || normalized(text) != copy_words(source, value.word_range)? {
            return Err(format!("Bound copy {} changed its page, original words, numerals or diacritics", value.copy_id));
        }
        if plan
            .commands
            .iter()
            .enumerate()
            .filter(|(_, action)| action.command == "object.label")
            .filter(|(index, action)| {
                action
                    .params
                    .get("ids")
                    .and_then(Value::as_array)
                    .is_some_and(|ids| ids.iter().filter_map(Value::as_str).any(|reference| direct_frame(reference, *index).ok() == Some(frame)))
            })
            .map(|(_, action)| action)
            .next_back()
            .and_then(|action| action.params.get("label"))
            .and_then(Value::as_str)
            != Some(label)
        {
            return Err("Host copy provenance was overwritten after binding".into());
        }
        if plan
            .commands
            .iter()
            .skip(label_index.saturating_add(1))
            .any(|action| action.command == "object.label" && action.params.get("ids").is_none())
        {
            return Err("An implicit later label could overwrite host copy provenance; use explicit object references".into());
        }
        coverage.entry(value.copy_id).or_default().push(value.word_range);
    }
    complete_copy_ranges(&mut coverage, &sources)?;
    Ok(coverage.into_keys().collect())
}

pub(crate) fn source_copy_context(brief: &str, scope: BatchScope) -> Result<Value, String> {
    let sources = copy_sources(brief, scope)?;
    let mut ordered: Vec<_> = sources.iter().collect();
    ordered.sort_by_key(|(_, source)| (source.page, source.language == "ar", source.segment));
    let segments: Vec<_> = ordered
        .into_iter()
        .map(|(id, source)| {
            json!({"id":id,"page":source.page,"language":source.language,
        "text":source.text,"words":source.words.iter().enumerate().map(|(index,text)|json!({"index":index,"text":text})).collect::<Vec<_>>() })
        })
        .collect();
    let context = json!({"segments":segments,"range_convention":"word_range is zero-based [start,end), excluding end; words retain exact punctuation, numerals and Arabic diacritics",
        "binding_policy":"Optional copy_bindings reference segments[].id. Bind each native text frame once. If any part of a segment is bound, partition all its words exactly once across bound frames. The host injects original words, so placeholder frame text is replaced. Unbound segments must satisfy the existing strict literal copy gate. Do not supply host provenance labels."});
    if serde_json::to_vec(&context).map_err(|error| error.to_string())?.len() > MAX_BRIEF_BYTES {
        return Err("Indexed source copy context exceeds 1 MiB; reduce the internal page scope".into());
    }
    Ok(context)
}

pub(crate) fn copy_binding_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["frame","copy_id","word_range"],"properties":{
        "frame":{"type":"integer","minimum":0,"maximum":511,"description":"Index of an authored native text frame.create command"},
        "copy_id":{"type":"string","minLength":1,"maxLength":64},
        "word_range":{"type":"array","minItems":2,"maxItems":2,"items":{"type":"integer","minimum":0,"maximum":MAX_COPY_WORDS}}
    }})
}

pub(crate) fn compile_native_plan_for_brief(text: &str, scope: BatchScope, brief: &str) -> Result<ProductionPlan, String> {
    compile_native_content(text, scope, Some(brief))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ai24_dense_teen_activities_batch_format_without_changing_native_cells() {
        let mut plan = ProductionPlan { title: "Seventeen and eighteen".into(), commands: vec![], images: vec![] };
        let activities: Vec<_> = [10, 7, 10, 8]
            .into_iter()
            .enumerate()
            .map(|(index, quantity)| NativeActivity {
                page: 0,
                rect: [60.0, 100.0 + index as f64 * 60.0, 260.0, 140.0 + index as f64 * 60.0],
                frame: CountFrame::Ten,
                quantity,
                filled: true,
            })
            .collect();
        append_activities(&mut plan, BatchScope { start: 0, count: 1 }, &activities).unwrap();
        assert_eq!(plan.commands.len(), 162, "75 native cells/tokens need 150 creation/label actions plus 12 shared format actions");
        let mut session = designcraft_engine::Session::new();
        session.execute("file.new", &json!({"facingPages":false})).unwrap();
        session.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":false})).unwrap();
        let items = &session.doc().unwrap().doc.spreads[0].items;
        assert_eq!(items.len(), 75);
        assert_eq!(items.iter().filter(|i| i.fill.swatch == "[Black]").count(), 35);
        assert!(items.iter().all(|i| i.stroke.swatch == "[Black]" && (i.stroke.weight - 0.8).abs() < 0.0001));
        assert_eq!(items.iter().filter(|i| i.label.contains(".cell.")).count(), 40);
    }

    #[test]
    fn ai24_anchor_images_rewrites_repeated_unit_placement_to_single_isolated_subject() {
        let mut brief = (1..14)
            .map(|n| format!("### PAGE {n:02} — Earlier approved lesson\n* **English Print Copy (LTR):**\n  > Earlier copy\n"))
            .collect::<String>();
        brief.push_str("### PAGE 14 — Fourteen fish\n* **English Print Copy (LTR):**\n  > 14 = 10 + 4\n* **Visual & Illustration Prompt (Art Direction):** 14 colorful reef fish arranged in a group of 10 and a group of 4.\n");
        let scope = BatchScope { start: 13, count: 1 };
        let mut plan = compile_native_plan(
            &json!({"title":"Fourteen", "commands":[], "images":[{
                "prompt":"One isolated reef fish", "page":13,"rect":[60,180,540,390],"label":"Fish", "repeat_groups":[10,4]
            }]})
            .to_string(),
            scope,
        )
        .unwrap();
        let value = serde_json::to_value(&plan.images[0]).unwrap();
        assert_eq!(value["repeat_rects"].as_array().unwrap().len(), 13);
        anchor_images(&brief, scope, &mut plan).unwrap();
        assert!(plan.images[0].prompt.contains("1 OF 14 NATIVE FRAMES"));
        assert!(plan.images[0].prompt.contains("EXACTLY ONE single isolated subject"));
        assert!(!plan.images[0].prompt.contains("Preserve the supplied entities and requested quantities"));
        assert!(plan.images[0].prompt.contains("human visual proof") || plan.images[0].prompt.contains("Human visual proof"));
    }

    const SCRIPT: &str = "# Global print specs\nA4; bleed 3 mm; vocalized Arabic.\n### PAGE 01 — Cover\n* **English Print Copy (LTR):**\n  > MY MATH BOOK\n* **Arabic Print Copy (RTL - Vocalized):**\n  > كِتَابُ الرِّيَاضِيَّاتِ\n* **Visual & Illustration Prompt (Art Direction):** Noor and Leo with a lion.\n### PAGE 02 — Ownership & Number Chart\n* **English Print Copy (LTR):**\n  > THIS BOOK BELONGS TO: [Name Line] | MY NUMBER CHART 1–20\n* **Visual & Illustration Prompt (Art Direction):** Twenty number cards.\n";

    fn plan(page: u32, en: &str, ar: &str) -> ProductionPlan {
        ProductionPlan {
            title: "Native proof".into(),
            commands: vec![
                EngineAction { command: "frame.create".into(), params: json!({"spread":page,"content":"text","rect":[45,45,550,200],"text":en}) },
                EngineAction { command: "frame.create".into(), params: json!({"spread":page,"content":"text","rect":[45,210,550,400],"text":ar}) },
            ],
            images: vec![],
        }
    }

    const BINDING_SCRIPT: &str = "### PAGE 01 — Cover\n* **English Print Copy (LTR):**\n  > Level KG2 (Ages 5–6) | Price $5\n* **Arabic Print Copy (RTL - Vocalized):**\n  > الرَّوْضَةُ الثَّانِيَةُ (KG2) - الأَعْمَارُ ٥-٦ سَنَوَاتٍ\n";

    fn binding_content() -> Value {
        json!({"title":"Bound copy","commands":[
            {"command":"frame.create","params":{"spread":0,"content":"text","rect":[45,45,250,90],"text":"placeholder"}},
            {"command":"frame.create","params":{"spread":0,"content":"text","rect":[250,45,550,140],"text":"placeholder"}},
            {"command":"frame.create","params":{"spread":0,"content":"text","rect":[45,100,250,145],"text":"placeholder"}},
            {"command":"frame.create","params":{"spread":0,"content":"text","rect":[45,160,250,205],"text":"placeholder"}}
        ],"images":[],"copy_bindings":[
            {"frame":0,"copy_id":"p1.en.1","word_range":[0,2]},
            {"frame":1,"copy_id":"p1.ar.1","word_range":[0,7]},
            {"frame":2,"copy_id":"p1.en.1","word_range":[2,4]},
            {"frame":3,"copy_id":"p1.en.2","word_range":[0,2]}
        ]})
    }

    #[test]
    fn copy_bindings_inject_original_words_across_interleaved_frames_and_survive_cache_roundtrip() {
        let scope = BatchScope { start: 0, count: 1 };
        let bound = compile_native_plan_for_brief(&binding_content().to_string(), scope, BINDING_SCRIPT).unwrap();
        assert_eq!(bound.commands[0].params["text"], "Level KG2");
        assert_eq!(bound.commands[1].params["text"], "الرَّوْضَةُ الثَّانِيَةُ (KG2) - الأَعْمَارُ ٥-٦ سَنَوَاتٍ");
        assert_eq!(bound.commands[2].params["text"], "(Ages 5–6)");
        assert_eq!(bound.commands[3].params["text"], "Price $$5");
        assert!(bound.commands.iter().any(|action| action.command == "object.label"));
        let cached: ProductionPlan = serde_json::from_value(serde_json::to_value(&bound).unwrap()).unwrap();
        validate_plan(BINDING_SCRIPT, scope, &cached).unwrap();
        let mut changed = cached.clone();
        changed.commands.push(EngineAction { command: "story.setText".into(), params: json!({"story":"$2.story","text":"(Ages 5–7)"}) });
        assert!(validate_plan(BINDING_SCRIPT, scope, &changed).is_err());
        let mut unvocalized = cached.clone();
        unvocalized.commands[1].params["text"] = json!("الروضة الثانية (KG2) - الأعمار ٥-٦ سنوات");
        assert!(validate_plan(BINDING_SCRIPT, scope, &unvocalized).is_err());
        assert!(validate_plan(&BINDING_SCRIPT.replace("5–6", "5–7"), scope, &cached).is_err());
    }

    #[test]
    fn copy_bindings_context_exposes_stable_source_words_and_bounded_schema() {
        let context = source_copy_context(BINDING_SCRIPT, BatchScope { start: 0, count: 1 }).unwrap();
        let segments = context["segments"].as_array().unwrap();
        let en = segments.iter().find(|source| source["id"] == "p1.en.1").unwrap();
        assert_eq!(en["words"], json!([{"index":0,"text":"Level"},{"index":1,"text":"KG2"},{"index":2,"text":"(Ages"},{"index":3,"text":"5–6)"}]));
        assert_eq!(copy_binding_schema()["properties"]["word_range"]["minItems"], 2);
    }

    #[test]
    fn copy_bindings_reject_gaps_duplicate_coverage_invalid_references_and_wrong_pages() {
        let scope = BatchScope { start: 0, count: 1 };
        for binding in [
            json!({"frame":0,"copy_id":"p1.en.1","word_range":[0,1]}),
            json!({"frame":0,"copy_id":"p1.en.1","word_range":[0,3]}),
            json!({"frame":999,"copy_id":"p1.en.1","word_range":[0,2]}),
            json!({"frame":0,"copy_id":"p2.en.1","word_range":[0,2]}),
            json!({"frame":0,"copy_id":"p1.en.1","word_range":[2,1]}),
            json!({"frame":0,"copy_id":"p1.en.1","word_range":[0,999999]}),
        ] {
            let mut content = binding_content();
            content["copy_bindings"][0] = binding;
            assert!(compile_native_plan_for_brief(&content.to_string(), scope, BINDING_SCRIPT).is_err());
        }
        let mut wrong_page = binding_content();
        wrong_page["commands"][0]["params"]["spread"] = json!(1);
        assert!(compile_native_plan_for_brief(&wrong_page.to_string(), scope, BINDING_SCRIPT).is_err());
        let mut duplicate_frame = binding_content();
        duplicate_frame["copy_bindings"][2]["frame"] = json!(0);
        assert!(compile_native_plan_for_brief(&duplicate_frame.to_string(), scope, BINDING_SCRIPT).is_err());
        assert!(compile_native_plan(&binding_content().to_string(), scope).is_err());
    }

    #[test]
    fn copy_bindings_keep_twenty_native_chart_cards_independent_of_creation_order() {
        let names =
            "One Two Three Four Five Six Seven Eight Nine Ten Eleven Twelve Thirteen Fourteen Fifteen Sixteen Seventeen Eighteen Nineteen Twenty";
        let brief = format!(
            "### PAGE 01 — Number Chart\n* **English Print Copy (LTR):**\n  > MY NUMBER CHART 1–20 ({})\n",
            names.split_whitespace().collect::<Vec<_>>().join(", ")
        );
        let mut content = json!({"title":"Twenty cards","commands":[{"command":"frame.create","params":{"spread":0,"content":"text","rect":[45,45,550,90],"text":"placeholder"}}],"images":[],"copy_bindings":[{"frame":0,"copy_id":"p1.en.1","word_range":[0,4]}]});
        for index in 0..20 {
            let commands = content["commands"].as_array_mut().unwrap();
            let frame = commands.len();
            commands.push(json!({"command":"frame.create","params":{"spread":0,"content":"text","rect":[45,100,100,145],"text":"placeholder"}}));
            commands.push(json!({"command":"frame.create","params":{"spread":0,"content":"text","rect":[105,100,160,145],"text":"بِطَاقَةٌ"}}));
            content["copy_bindings"].as_array_mut().unwrap().push(json!({"frame":frame,"copy_id":"p1.en.1","word_range":[index+4,index+5]}));
        }
        let scope = BatchScope { start: 0, count: 1 };
        let plan = compile_native_plan_for_brief(&content.to_string(), scope, &brief).unwrap();
        validate_plan(&brief, scope, &plan).unwrap();
        assert_eq!(plan.commands[1].params["text"], "(One,");
        assert_eq!(plan.commands[39].params["text"], "Twenty)");
    }

    #[test]
    fn copy_bindings_reject_forged_or_overwritten_provenance_and_keep_unbound_copy_strict() {
        let scope = BatchScope { start: 0, count: 1 };
        let bound = compile_native_plan_for_brief(&binding_content().to_string(), scope, BINDING_SCRIPT).unwrap();
        assert!(validate_plan(&format!("# Changed global specification\n{BINDING_SCRIPT}"), scope, &bound).is_err());
        let mut forged = binding_content();
        forged["commands"].as_array_mut().unwrap().push(serde_json::to_value(&bound.commands[4]).unwrap());
        assert!(compile_native_plan_for_brief(&forged.to_string(), scope, BINDING_SCRIPT).is_err());
        let mut overwritten = bound.clone();
        overwritten
            .commands
            .push(EngineAction { command: "object.label".into(), params: json!({"ids":["$0.id","$2.id"],"label":"lost provenance"}) });
        assert!(validate_plan(BINDING_SCRIPT, scope, &overwritten).is_err());
        let mut partial = binding_content();
        partial["copy_bindings"].as_array_mut().unwrap().pop();
        partial["commands"][3]["params"]["text"] = json!("Price $15");
        let plan = compile_native_plan_for_brief(&partial.to_string(), scope, BINDING_SCRIPT).unwrap();
        assert!(validate_plan(BINDING_SCRIPT, scope, &plan).is_err());
        let mut too_many = binding_content();
        too_many["copy_bindings"] = json!(vec![too_many["copy_bindings"][0].clone(); MAX_COPY_BINDINGS + 1]);
        assert!(compile_native_plan_for_brief(&too_many.to_string(), scope, BINDING_SCRIPT).unwrap_err().contains("256"));
    }

    #[test]
    fn facing_context_page_twelve_keeps_thirteen_as_context_across_review_boundary() {
        let brief: String = (1..=14).map(|page| {
            let title = match page { 12 => "Unit 2 Opener: Ocean Counting", 13 => "Ocean Counting Practice", _ => "Independent Lesson" };
            format!("### PAGE {page:02} — {title}\n* **English Print Copy (LTR):**\n  > Lesson {page}\n* **Visual & Illustration Prompt (Art Direction):** Ocean coral scene {page} — رِيَاضِيَّات\n")
        }).collect();
        let scope = BatchScope { start: 11, count: 1 };
        let context = facing_context(&brief, scope).unwrap();
        assert_eq!(context["scope"], json!({"start":11,"count":1}));
        assert_eq!(context["produce_only_pages"], json!([11]));
        assert_eq!(context["pairs"].as_array().unwrap().len(), 1);
        let pair = &context["pairs"][0];
        assert_eq!(pair["pair_id"], "pages-0012-0013");
        assert_eq!(pair["human_pages"], json!([12, 13]));
        assert_eq!(pair["members"][0]["page"], 11);
        assert_eq!(pair["members"][0]["role"], "produce");
        assert_eq!(pair["members"][1]["page"], 12);
        assert_eq!(pair["members"][1]["role"], "context_only");
        assert!(pair["members"][1]["full_script"].as_str().unwrap().contains("رِيَاضِيَّات"));
        assert_eq!(pair["linked_candidate"], true);
        let reverse = facing_context(&brief, BatchScope { start: 12, count: 1 }).unwrap();
        assert_eq!(reverse["pairs"][0]["pair_id"], pair["pair_id"]);
        assert_eq!(reverse["produce_only_pages"], json!([12]));
        let review = facing_context(&brief, BatchScope { start: 0, count: 12 }).unwrap();
        assert_eq!(review["produce_only_pages"].as_array().unwrap().len(), 12);
        assert_eq!(review["pairs"][6]["members"][1]["role"], "context_only");
    }

    #[test]
    fn facing_context_keeps_cover_standalone_and_has_no_invented_fallback_pages() {
        let cover = facing_context(SCRIPT, BatchScope { start: 0, count: 1 }).unwrap();
        assert_eq!(cover["pairs"][0]["pair_id"], "cover-0001");
        assert_eq!(cover["pairs"][0]["members"].as_array().unwrap().len(), 1);
        assert_eq!(cover["pairs"][0]["linked_candidate"], false);
        let generic = facing_context("Design an English/Arabic educational magazine", BatchScope { start: 11, count: 1 }).unwrap();
        assert_eq!(generic["status"], "no_explicit_page_script");
        assert!(generic["pairs"].as_array().unwrap().is_empty());
        assert!(facing_context(SCRIPT, BatchScope { start: u32::MAX, count: 1 }).is_err());
    }

    #[test]
    fn facing_context_does_not_force_linking_unrelated_lessons() {
        let brief = "### PAGE 01 — Cover\n### PAGE 02 — Measuring Length\n### PAGE 03 — Shapes Everywhere\n";
        let context = facing_context(brief, BatchScope { start: 1, count: 1 }).unwrap();
        assert_eq!(context["pairs"][0]["linked_candidate"], false);
        assert_eq!(context["pairs"][0]["link_reason"], "independent_lessons");
        assert_eq!(context["pairs"][0]["members"][1]["role"], "context_only");
    }

    #[test]
    fn scoped_prompt_retains_global_specs_and_only_requested_page() {
        let scoped = scoped_prompt(SCRIPT, BatchScope { start: 1, count: 1 }).unwrap();
        assert!(scoped.contains("A4; bleed 3 mm"));
        assert!(scoped.contains("### PAGE 02 — Ownership"));
        assert!(!scoped.contains("### PAGE 01"));
        assert!(!scoped.contains("Noor and Leo with a lion"));
        let contract = parse(SCRIPT).unwrap().unwrap();
        assert_eq!(contract.pages.len(), 2);
        assert_eq!(contract.pages[0].arabic_print_copy.as_deref(), Some("كِتَابُ الرِّيَاضِيَّاتِ"));
    }

    #[test]
    fn explicit_copy_rejects_changed_page_missing_arabic_and_removed_diacritics() {
        let scope = BatchScope { start: 0, count: 1 };
        validate_plan(SCRIPT, scope, &plan(0, "MY MATH BOOK", "كِتَابُ الرِّيَاضِيَّاتِ")).unwrap();
        assert!(validate_plan(SCRIPT, scope, &plan(0, "MY SCIENCE BOOK", "كِتَابُ الرِّيَاضِيَّاتِ")).is_err());
        assert!(validate_plan(SCRIPT, scope, &plan(0, "MY MATH BOOK", "كتاب الرياضيات")).is_err());
        assert!(validate_plan(SCRIPT, scope, &plan(1, "MY MATH BOOK", "كِتَابُ الرِّيَاضِيَّاتِ")).is_err());
        validate_plan(SCRIPT, BatchScope { start: 1, count: 1 }, &plan(1, "THIS BOOK BELONGS TO:\n_______", "MY NUMBER CHART 1–20")).unwrap();
    }

    #[test]
    fn exact_copy_does_not_match_a_different_quantity_or_overwritten_story() {
        let brief = "### PAGE 01 — Counting\n* **English Print Copy (LTR):**\n  > 5 Monkeys\n";
        let scope = BatchScope { start: 0, count: 1 };
        assert!(validate_plan(brief, scope, &plan(0, "15 Monkeys", "")).is_err());
        let mut changed = plan(0, "5 Monkeys", "");
        changed.commands.push(EngineAction { command: "story.setText".into(), params: json!({"story":"$0.story","text":"5 Lions"}) });
        assert!(validate_plan(brief, scope, &changed).is_err());
    }

    #[test]
    fn explicit_script_parser_rejects_duplicate_gap_zero_and_hostile_unicode_headings() {
        for brief in [
            "### PAGE 00 — Zero",
            "### PAGE 01 — A\n### PAGE 01 — B",
            "### PAGE 01 — A\n### PAGE 03 — Gap",
            "### PAGE 999999999999999999999999999999 — Huge",
            "### PAGE 1ع — Broken",
        ] {
            assert!(parse(brief).is_err(), "{brief}");
        }
        assert!(parse("Generic Arabic brief رِيَاضِيَّات").unwrap().is_none());
        assert!(scoped_prompt(SCRIPT, BatchScope { start: u32::MAX, count: 1 }).is_err());
    }

    #[test]
    fn deterministic_count_frames_have_exact_cell_and_token_counts_and_atomic_failure() {
        let mut native = ProductionPlan { title: "Count".into(), commands: vec![], images: vec![] };
        let activity: NativeActivity =
            serde_json::from_value(json!({"page":0,"rect":[50,100,300,200],"frame":"ten","quantity":7,"filled":true})).unwrap();
        append_activities(&mut native, BatchScope { start: 0, count: 1 }, &[activity]).unwrap();
        assert_eq!(
            native.commands.iter().filter(|a| a.params.get("label").and_then(Value::as_str).is_some_and(|label| label.contains(".cell."))).count(),
            10
        );
        assert_eq!(
            native.commands.iter().filter(|a| a.params.get("label").and_then(Value::as_str).is_some_and(|label| label.contains(".token."))).count(),
            7
        );
        let before = serde_json::to_value(&native).unwrap();
        let invalid: NativeActivity =
            serde_json::from_value(json!({"page":0,"rect":[50,100,300,200],"frame":"five","quantity":6,"filled":true})).unwrap();
        assert!(append_activities(&mut native, BatchScope { start: 0, count: 1 }, &[invalid]).is_err());
        assert_eq!(serde_json::to_value(&native).unwrap(), before);
        let brief = "### PAGE 01 — Solid 3D Shapes\n* **English Print Copy (LTR):**\n  > Cone (like a party hat)\n* **Arabic Print Copy (RTL - Vocalized):**\n  > مخروط (مثل قبعة الحفلة)\n";
        let scope = BatchScope { start: 0, count: 1 };
        let raw = json!({
            "title": "Page 1 — Solid 3D Shapes",
            "commands": [
                {"command":"frame.create","params":{"spread":0,"content":"text","rect":[45,45,280,100],"text":"Cone (like a party hat)"}},
                {"command":"type.char","params":{"attrs":{"size":16}}},
                {"command":"type.para","params":{"attrs":{"align":"left","direction":"leftToRight"}}},
                {"command":"frame.create","params":{"spread":0,"content":"unassigned","rect":[45,120,280,220]}},
                {"command":"frame.create","params":{"spread":0,"content":"text","rect":[300,45,550,100],"text":"placeholder"}},
                {"command":"object.stroke","params":{"ids":["$2.id"],"swatch":"[Black]","weight":1.0}},
                {"command":"object.textFrameOptions","params":{"ids":["$1.id"],"inset":[4,4,4,4]}}
            ],
            "images": [],
            "copy_bindings": [
                {"frame": 4, "copy_id": "p1.ar.1", "word_range": [0, 3]}
            ]
        });
        let plan = compile_native_plan_for_brief(&raw.to_string(), scope, brief).unwrap();
        assert_eq!(plan.commands[4].params["text"], "مخروط (مثل قبعة الحفلة)");
        assert_eq!(plan.commands[5].params["ids"], json!(["$3.id"]));
        assert_eq!(plan.commands[6].params["ids"], json!(["$0.id"]));
        validate_plan(brief, scope, &plan).unwrap();
        let mut session = designcraft_engine::Session::new();
        session.execute("file.new", &json!({"facingPages":false})).unwrap();
        session.execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":false})).unwrap();
    }

    #[test]
    fn twenty_count_frame_and_copy_binding_formatting_index_normalize_cleanly() {
        let mut native = ProductionPlan { title: "Fourteen".into(), commands: vec![], images: vec![] };
        let twenty: NativeActivity =
            serde_json::from_value(json!({"page":0,"rect":[50,100,300,260],"frame":"twenty","quantity":14,"filled":true})).unwrap();
        append_activities(&mut native, BatchScope { start: 0, count: 1 }, &[twenty]).unwrap();
        assert_eq!(
            native
                .commands
                .iter()
                .filter(|a| a.params.get("label").and_then(Value::as_str).is_some_and(|label| label.contains(".twenty.cell.")))
                .count(),
            20
        );
        assert_eq!(
            native
                .commands
                .iter()
                .filter(|a| a.params.get("label").and_then(Value::as_str).is_some_and(|label| label.contains(".twenty.token.")))
                .count(),
            14
        );
        let invalid: NativeActivity =
            serde_json::from_value(json!({"page":0,"rect":[50,100,300,260],"frame":"twenty","quantity":21,"filled":true})).unwrap();
        assert!(append_activities(&mut native, BatchScope { start: 0, count: 1 }, &[invalid]).is_err());

        let brief = "### PAGE 01 — Fourteen\n* **English Print Copy (LTR):**\n  > Fourteen (10 + 4)\n* **Arabic Print Copy (RTL - Vocalized):**\n  > أَرْبَعَةَ عَشَرَ\n";
        let scope = BatchScope { start: 0, count: 1 };
        let raw = json!({
            "title": "Page 1 — Fourteen",
            "commands": [
                {"command":"frame.create","params":{"spread":0,"content":"text","rect":[45,45,280,100],"text":"placeholder"}},
                {"command":"type.char","params":{"attrs":{"size":18}}},
                {"command":"frame.create","params":{"spread":0,"content":"text","rect":[300,45,550,100],"text":"placeholder"}},
                {"command":"type.para","params":{"attrs":{"align":"right","direction":"rightToLeft"}}}
            ],
            "images": [],
            "activities": [{"page":0,"rect":[50,120,300,280],"frame":"twenty","quantity":14,"filled":true}],
            "copy_bindings": [
                {"frame": 1, "copy_id": "p1.en.1", "word_range": [0, 1]},
                {"frame": 3, "copy_id": "p1.ar.1", "word_range": [0, 1]}
            ]
        });
        let plan = compile_native_plan_for_brief(&raw.to_string(), scope, brief).unwrap();
        assert_eq!(plan.commands[0].params["text"], "Fourteen (10 + 4)");
        assert_eq!(plan.commands[2].params["text"], "أَرْبَعَةَ عَشَرَ");
        validate_plan(brief, scope, &plan).unwrap();

        let grouped = json!({
            "title": "Page 1 — Fourteen grouped",
            "commands": [
                {"command":"frame.create","params":{"spread":0,"content":"text","rect":[45,45,280,100],"text":"placeholder"}},
                {"command":"frame.create","params":{"spread":0,"content":"text","rect":[300,45,550,100],"text":"placeholder"}},
                {"command":"type.char","params":{"attrs":{"size":18}}},
                {"command":"type.para","params":{"attrs":{"align":"right","direction":"rightToLeft"}}}
            ],
            "images": [],
            "copy_bindings": [
                {"frame": 2, "copy_id": "p1.en.1", "word_range": [0, 1]},
                {"frame": 3, "copy_id": "p1.ar.1", "word_range": [0, 1]}
            ]
        });
        let grouped_plan = compile_native_plan_for_brief(&grouped.to_string(), scope, brief).unwrap();
        assert_eq!(grouped_plan.commands[0].params["text"], "Fourteen (10 + 4)");
        assert_eq!(grouped_plan.commands[1].params["text"], "أَرْبَعَةَ عَشَرَ");
        validate_plan(brief, scope, &grouped_plan).unwrap();

        let rich_refs = json!({
            "title": "Alias and result reference preservation",
            "commands": [
                {"command":"frame.create","params":{"spread":0,"rect":[45,45,240,95],"text":"Implicit text content"}},
                {"command":"type.char","params":{"story":"$0.story","attrs":{"size":14}}},
                {"command":"qr.create","params":{"spread":0,"rect":[260,45,330,115],"text":"https://example.org"}},
                {"command":"object.label","params":{"ids":["$2.id"],"label":"qr-badge"}},
                {"command":"frame.create","params":{"spread":0,"rect":[45,130,145,210],"content":"unassigned"}},
                {"command":"frame.create","params":{"spread":0,"rect":[95,150,175,230],"content":"unassigned"}},
                {"command":"pathfinder.subtract","params":{"ids":["$4.id","$5.id"]}},
                {"command":"object.fill","params":{"ids":["$6.id"],"swatch":"Black"}},
                {"command":"text.thread","params":{"from":"$0.id","spread":0,"rect":[45,250,240,310]}},
                {"command":"text.select","params":{"story":"$8.story","anchor":0,"focus":8}}
            ],
            "images": []
        });
        let compiled_rich = compile_native_plan(&rich_refs.to_string(), scope).unwrap();
        assert_eq!(compiled_rich.commands[0].params["content"], "text");
        assert_eq!(compiled_rich.commands[1].params["story"], "$0.story");
        assert_eq!(compiled_rich.commands[2].command, "object.qrCode");
        assert_eq!(compiled_rich.commands[3].params["ids"], json!(["$2.id"]));
        assert_eq!(compiled_rich.commands[6].command, "object.pathfinder");
        assert_eq!(compiled_rich.commands[7].params["ids"], json!(["$6.id"]));
        assert_eq!(compiled_rich.commands[9].params["story"], "$8.story");

        // A container shape.create between an earlier header story and its own inner text frame
        // must rewrite $2.story to the paired inner story ($3.story), not the unrelated header ($0.story).
        let paired_container_refs = json!({
            "title": "Paired container forward inner story",
            "commands": [
                {"command":"frame.create","params":{"spread":0,"content":"text","rect":[45,45,300,80],"text":"Header story"}},
                {"command":"type.para","params":{"attrs":{"align":"center"}}},
                {"command":"shape.create","params":{"spread":0,"shape":"rect","rect":[45,100,300,170]}},
                {"command":"frame.create","params":{"spread":0,"content":"text","rect":[55,110,290,160],"text":"Inner card story"}},
                {"command":"text.select","params":{"story":"$2.story","anchor":0,"focus":5}},
                {"command":"type.char","params":{"attrs":{"size":13}}}
            ],
            "images": []
        });
        let compiled_paired = compile_native_plan(&paired_container_refs.to_string(), scope).unwrap();
        assert_eq!(compiled_paired.commands[4].params["story"], "$3.story");
    }

    #[test]
    fn arabic_page_headings_and_indic_digits_parse_without_false_positive_prose() {
        let ar_md = "## الصفحة ١ — غِلَافُ الكِتَابِ\n* **النص العربي:**\n  > مَرْحَبًا بِأَطْفَالِ الرَّوْضَةِ\n## صفحة 2: الحُرُوفُ\n* **النص العربي:**\n  > أ ب ت\n";
        let parsed = parse(ar_md).unwrap().unwrap();
        assert_eq!(parsed.pages.len(), 2);
        assert_eq!(parsed.pages[0].page, 0);
        assert_eq!(parsed.pages[0].title, "غِلَافُ الكِتَابِ");
        assert_eq!(parsed.pages[0].arabic_print_copy.as_deref(), Some("مَرْحَبًا بِأَطْفَالِ الرَّوْضَةِ"));
        assert_eq!(parsed.pages[1].page, 1);
        assert_eq!(parsed.pages[1].title, "الحُرُوفُ");

        let ar_bare = "صفحة 1: المقدمة\n* **النص العربي:**\n  > بِسْمِ اللَّهِ\nصفحة ٢: التمارين\n* **النص العربي:**\n  > نَشَاطٌ أَوَّلُ\n";
        let parsed_bare = parse(ar_bare).unwrap().unwrap();
        assert_eq!(parsed_bare.pages.len(), 2);
        assert_eq!(parsed_bare.pages[1].page, 1);

        // Casual prose mentioning "صفحة 1" without script structure must stay None.
        assert!(parse("صمم لي صفحة 1 جميلة عن الحروف الهجائية للأطفال").unwrap().is_none());

        // Concatenated JSON plans or trailing tokens must be rejected by compile_native_plan.
        let valid_plan = json!({
            "title": "Single valid plan",
            "commands": [{"command": "frame.create", "params": {"spread": 0, "content": "text", "rect": [45, 45, 300, 90], "text": "Hello"}}],
            "images": []
        });
        let concatenated = format!("{valid_plan} {valid_plan}");
        let err = compile_native_plan(&concatenated, BatchScope { start: 0, count: 1 }).unwrap_err();
        assert!(err.contains("Invalid native content plan"), "{err}");
    }
}
