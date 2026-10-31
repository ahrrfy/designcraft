use super::*;

pub(super) const MAX_REQUEST_ALLOWANCE: u32 = 10_000;
pub(super) const MAX_TOKEN_LIMIT: u64 = 16_000_000;

pub(crate) fn is_non_text_model(model: &str) -> bool {
    designcraft_ai_types::is_non_text_model(model)
}

/// Never persisted, and never included in a debug dump.
#[derive(Clone, Default)]
pub(super) struct Secret(pub(super) String);

impl Drop for Secret {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

pub(super) fn collect_page_ranges(pages: usize, mut predicate: impl FnMut(usize) -> bool) -> Vec<[usize; 2]> {
    let mut ranges: Vec<[usize; 2]> = Vec::new();
    for page in 0..pages {
        if predicate(page) {
            let one_based = page.saturating_add(1);
            if let Some(last) = ranges.last_mut().filter(|last| last[1].saturating_add(1) == one_based) {
                last[1] = one_based;
            } else {
                ranges.push([one_based, one_based]);
            }
        }
    }
    ranges
}

pub(super) fn format_page_ranges(ranges: &[[usize; 2]], arabic: bool) -> String {
    if ranges.is_empty() {
        return text(arabic, "none", "لا شيء").to_string();
    }
    ranges.iter().map(|[start, end]| if start == end { start.to_string() } else { format!("{start}–{end}") }).collect::<Vec<_>>().join(", ")
}

pub(super) struct OpenDocument {
    pub(super) index: usize,
    pub(super) uid: u64,
    pub(super) path: Option<String>,
    pub(super) title: String,
    pub(super) facing_pages: bool,
    pub(super) right_to_left_binding: bool,
    pub(super) pages: usize,
    pub(super) remaining_pages: usize,
    pub(super) continuable: bool,
    pub(super) filled_ranges: Vec<[usize; 2]>,
    pub(super) blank_ranges: Vec<[usize; 2]>,
}
impl OpenDocument {
    pub(super) fn from_document(index: usize, document: &designcraft_engine::DocState) -> Self {
        let pages = document.doc.page_count();
        let first_blank = (0..pages).find(|page| !document.doc.page_has_items(*page));
        let filled_ranges = collect_page_ranges(pages, |page| document.doc.page_has_items(page));
        let blank_ranges = collect_page_ranges(pages, |page| !document.doc.page_has_items(page));
        Self {
            index,
            uid: document.uid,
            path: document.path.clone(),
            title: document.title(),
            facing_pages: document.doc.settings.facing_pages,
            right_to_left_binding: document.doc.settings.right_to_left_binding,
            pages,
            remaining_pages: first_blank.map_or(0, |first| pages.saturating_sub(first)),
            continuable: first_blank.is_some_and(|first| first > 0 && (first..pages).all(|page| !document.doc.page_has_items(page))),
            filled_ranges,
            blank_ranges,
        }
    }

    pub(super) fn effective_filled_ranges(&self) -> Vec<[usize; 2]> {
        if !self.filled_ranges.is_empty() || !self.blank_ranges.is_empty() {
            return self.filled_ranges.clone();
        }
        let completed = self.pages.saturating_sub(self.remaining_pages);
        if completed > 0 { vec![[1, completed]] } else { Vec::new() }
    }

    pub(super) fn effective_blank_ranges(&self) -> Vec<[usize; 2]> {
        if !self.filled_ranges.is_empty() || !self.blank_ranges.is_empty() {
            return self.blank_ranges.clone();
        }
        let completed = self.pages.saturating_sub(self.remaining_pages);
        if self.remaining_pages > 0 { vec![[completed.saturating_add(1), self.pages]] } else { Vec::new() }
    }

    pub(super) fn page_map_line(&self, arabic: bool) -> String {
        let filled = format_page_ranges(&self.effective_filled_ranges(), arabic);
        let blank = format_page_ranges(&self.effective_blank_ranges(), arabic);
        if arabic {
            format!("خريطة الصفحات — المنجزة: {filled} · الفارغة: {blank}")
        } else {
            format!("Page map — Completed: {filled} · Blank: {blank}")
        }
    }

    pub(super) fn has_interior_gaps(&self) -> bool {
        let filled = self.effective_filled_ranges();
        let blank = self.effective_blank_ranges();
        if filled.is_empty() || blank.is_empty() {
            return false;
        }
        if filled.len() != 1 || blank.len() != 1 {
            return true;
        }
        let [f_start, f_end] = filled[0];
        let [b_start, b_end] = blank[0];
        f_start != 1 || b_start != f_end.saturating_add(1) || b_end != self.pages
    }

    pub(super) fn can_continue_with_target(&self, requested_pages: Option<u32>, has_prompt: bool) -> bool {
        if self.continuable {
            return true;
        }
        if self.has_interior_gaps() || !has_prompt {
            return false;
        }
        let filled = self.effective_filled_ranges();
        let blank = self.effective_blank_ranges();
        if filled.is_empty() {
            return true;
        }
        if blank.is_empty() {
            return requested_pages.is_some_and(|req| u32::try_from(self.pages).ok().is_some_and(|cur| req > cur));
        }
        false
    }

    pub(super) fn effective_remaining_pages(&self, requested_pages: Option<u32>) -> Option<u32> {
        let cur_pages = u32::try_from(self.pages).ok()?;
        let completed = self.pages.saturating_sub(self.remaining_pages);
        let completed_u32 = u32::try_from(completed).ok()?;
        let total = match requested_pages {
            Some(req) if req < cur_pages => return None,
            Some(req) => req.max(cur_pages),
            None => cur_pages,
        };
        let rem = total.saturating_sub(completed_u32);
        (rem > 0).then_some(rem)
    }

    pub(super) fn continuation_guidance(&self, arabic: bool, requested_pages: Option<u32>, has_prompt: bool) -> Option<String> {
        if self.can_continue_with_target(requested_pages, has_prompt) {
            let cur_pages = u32::try_from(self.pages).ok()?;
            let filled = self.effective_filled_ranges();
            if filled.is_empty() {
                let target = requested_pages.unwrap_or(cur_pages).max(cur_pages);
                if target > cur_pages {
                    return Some(if arabic {
                        format!("سيُوسَّع المستند الفارغ تلقائياً من {} إلى {target} صفحة ويبدأ الإنتاج من الصفحة 1.", self.pages)
                    } else {
                        format!("Blank document will automatically expand from {} to {target} pages and start at page 1.", self.pages)
                    });
                }
                return Some(
                    text(arabic, "Blank document is ready; production will start at page 1.", "المستند الفارغ جاهز؛ سيبدأ الإنتاج من الصفحة 1.")
                        .to_string(),
                );
            }
            if let Some(target) = requested_pages.filter(|req| *req > cur_pages) {
                let next_page = self.pages.saturating_sub(self.remaining_pages).saturating_add(1);
                return Some(if arabic {
                    format!("سيُوسَّع الكتاب تلقائياً من {} إلى {target} صفحة ويُستكمل من الصفحة {next_page}.", self.pages)
                } else {
                    format!("Book will automatically expand from {} to {target} pages and continue from page {next_page}.", self.pages)
                });
            }
            return None;
        }
        self.non_continuable_reason(arabic)
    }

    pub(super) fn non_continuable_reason(&self, arabic: bool) -> Option<String> {
        if self.continuable {
            return None;
        }
        let filled = self.effective_filled_ranges();
        let blank = self.effective_blank_ranges();
        if filled.is_empty() {
            Some(
                text(
                    arabic,
                    "All pages are blank; enter instructions above to expand and produce from page 1, or choose New document.",
                    "جميع الصفحات فارغة؛ أدخل تعليمات الإنتاج أعلاه لتوسيع الصفحات والبدء من الصفحة 1، أو اختر مستنداً جديداً.",
                )
                .to_string(),
            )
        } else if blank.is_empty() {
            Some(
                text(
                    arabic,
                    "All pages already contain items; specify a larger total page count in the prompt to append and continue.",
                    "جميع الصفحات تحتوي على عناصر؛ حدد إجمالي صفحات أكبر في الوصف لإضافة صفحات جديدة واستكمالها.",
                )
                .to_string(),
            )
        } else if blank.first().is_some_and(|[start, _]| *start == 1) {
            Some(if arabic {
                format!(
                    "الصفحة الأولى فارغة بينما توجد صفحات ممتلئة لاحقاً ({})؛ يتطلب الاستكمال التلقائي بادئة مكتملة تبدأ من الصفحة 1.",
                    format_page_ranges(&filled, true)
                )
            } else {
                format!(
                    "Leading page 1 is blank while later pages contain items ({}); prefix continuation requires completed pages starting at page 1.",
                    format_page_ranges(&filled, false)
                )
            })
        } else {
            Some(if arabic {
                format!(
                    "توجد فجوة صفحات فارغة ({}) قبل صفحات ممتلئة ({})؛ يتطلب الاستكمال التلقائي صفحات مكتملة متصلة متبوعة بصفحات فارغة.",
                    format_page_ranges(&blank, true),
                    format_page_ranges(&filled, true)
                )
            } else {
                format!(
                    "Blank gap ({}) precedes filled pages ({}); prefix continuation requires a contiguous completed prefix followed by blank pages.",
                    format_page_ranges(&blank, false),
                    format_page_ranges(&filled, false)
                )
            })
        }
    }
}

pub(super) struct RequestEstimate {
    pub(super) review_pages: u32,
    pub(super) remaining_pages: u32,
    pub(super) remaining_groups: u32,
    pub(super) book_allowance: u32,
    pub(super) scripted: bool,
    pub(super) image_allowance: u32,
    pub(super) vision_allowance: u32,
}

#[derive(Clone, Copy)]
pub(super) struct TokenEstimate {
    pub(super) per_page_tokens: u64,
    pub(super) review_tokens: u64,
    pub(super) book_tokens: u64,
}

#[derive(Clone)]
pub(super) struct CompanionProof {
    pub(super) path: String,
    pub(super) texture: Option<egui::TextureHandle>,
    pub(super) error: String,
}

/// Global zero-based pages: the cover stands alone, followed by 2–3, 4–5, … .
pub(super) fn facing_proof_pair(page: u32, right_to_left: bool) -> Option<[u32; 2]> {
    if page == 0 {
        return None;
    }
    let first = if page.is_multiple_of(2) { page.checked_sub(1)? } else { page };
    let second = first.checked_add(1)?;
    Some(if right_to_left { [second, first] } else { [first, second] })
}

pub(super) fn automatic_proof_binding(brief: &str, fallback: bool) -> bool {
    let arabic = |character: char| {
        character.is_alphabetic()
            && matches!(u32::from(character), 0x0600..=0x06ff | 0x0750..=0x077f | 0x08a0..=0x08ff | 0xfb50..=0xfdff | 0xfe70..=0xfeff)
    };
    let mut content = None;
    for line in brief.lines() {
        let lower = line.to_lowercase();
        if lower.contains("binding") || lower.contains("اتجاه التجليد") || lower.contains("اتجاه الكتاب") {
            if ["righttoleft", "right-to-left", "rtl", "من اليمين"].iter().any(|word| lower.contains(*word)) {
                return true;
            }
            if ["lefttoright", "left-to-right", "ltr", "من اليسار"].iter().any(|word| lower.contains(*word)) {
                return false;
            }
        }
        if ["content copy language", "content language", "لغة المحتوى", "لغة الكتاب"].iter().any(|word| lower.contains(*word)) {
            let value = lower.split_once(':').map_or(lower.as_str(), |(_, value)| value);
            if ["arabic", "العربي", "بالعربي", "عربية"].iter().any(|word| value.contains(*word)) {
                content = Some(true);
            } else if ["english", "إنجليزي", "انجليزي", "إنكليزي", "انكليزي"].iter().any(|word| value.contains(*word)) {
                content = Some(false);
            } else if value.chars().any(arabic) {
                content = Some(true);
            }
        }
    }
    content.unwrap_or_else(|| {
        let lower = brief.to_lowercase();
        if ["باللغة الإنجليزية", "باللغة الانجليزية", "باللغة الإنكليزية", "باللغة الانكليزية", "english-only", "english only"]
            .iter()
            .any(|phrase| lower.contains(*phrase))
        {
            false
        } else if brief.chars().any(arabic) {
            true
        } else if brief.chars().any(|character| character.is_ascii_alphabetic()) {
            false
        } else {
            fallback
        }
    })
}
impl RequestEstimate {
    pub(super) fn allowance(&self) -> u32 {
        self.review_pages.saturating_add(self.image_allowance).saturating_add(self.vision_allowance)
    }
}

fn script_digit_ui(ch: char) -> Option<u32> {
    match ch {
        '0'..='9' => Some(u32::from(ch) - u32::from('0')),
        '\u{0660}'..='\u{0669}' => Some(u32::from(ch) - 0x0660),
        '\u{06F0}'..='\u{06F9}' => Some(u32::from(ch) - 0x06F0),
        _ => None,
    }
}

fn parse_script_number_ui(digits: &str) -> Option<u32> {
    if digits.is_empty() || digits.len() > 8 {
        return None;
    }
    let mut value = 0u32;
    for ch in digits.chars() {
        value = value.checked_mul(10)?.checked_add(script_digit_ui(ch)?)?;
    }
    (1..=1000).contains(&value).then_some(value)
}

fn strip_title_sep_ui(s: &str) -> Option<&str> {
    s.strip_prefix('—').or_else(|| s.strip_prefix('–')).or_else(|| s.strip_prefix('-')).or_else(|| s.strip_prefix(':'))
}

fn parse_script_heading_ui(line: &str) -> Result<Option<(u32, u32)>, ()> {
    let line = line.trim();
    let (heading, is_markdown) = if line.starts_with('#') { (line.trim_start_matches('#').trim_start(), true) } else { (line, false) };
    let (rest, is_arabic_kw) = if is_markdown && heading.get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("pages")) {
        let tail = heading.get(5..).ok_or(())?;
        if tail.is_empty() {
            return Err(());
        }
        if !tail.starts_with(char::is_whitespace) {
            return Ok(None);
        }
        (tail, false)
    } else if is_markdown && heading.get(..4).is_some_and(|prefix| prefix.eq_ignore_ascii_case("page")) {
        let tail = heading.get(4..).ok_or(())?;
        if tail.is_empty() {
            return Err(());
        }
        if !tail.starts_with(char::is_whitespace) {
            return Ok(None);
        }
        (tail, false)
    } else if let Some(rest) = heading
        .strip_prefix("الصفحات")
        .or_else(|| heading.strip_prefix("صفحات"))
        .or_else(|| heading.strip_prefix("الصفحة"))
        .or_else(|| heading.strip_prefix("صفحة"))
    {
        if !rest.starts_with(char::is_whitespace) {
            return Ok(None);
        }
        (rest, true)
    } else {
        return Ok(None);
    };
    let rest = rest.trim_start();
    let boundary = rest.find(|c: char| script_digit_ui(c).is_none()).unwrap_or(rest.len());
    let digits_str = rest.get(..boundary).ok_or(())?;
    if digits_str.is_empty() {
        return if is_markdown { Err(()) } else { Ok(None) };
    }
    let start_number = parse_script_number_ui(digits_str).ok_or(())?;
    let after_first = rest.get(boundary..).ok_or(())?;
    let mut end_number = start_number;
    let mut suffix = after_first.trim_start();

    let attached_range = after_first.starts_with('-') || after_first.starts_with('–');
    if let Some(after_dash) = suffix.strip_prefix('-').or_else(|| suffix.strip_prefix('–')) {
        let trimmed_dash = after_dash.trim_start();
        let end_boundary = trimmed_dash.find(|c: char| script_digit_ui(c).is_none()).unwrap_or(trimmed_dash.len());
        if let Some(end_digits) = trimmed_dash.get(..end_boundary).filter(|s| !s.is_empty()) {
            let trailing = trimmed_dash.get(end_boundary..).unwrap_or("").trim_start();
            let has_trailing_sep = strip_title_sep_ui(trailing).is_some() || (is_arabic_kw && is_markdown && trailing.is_empty());
            if attached_range && after_dash.starts_with(|c| script_digit_ui(c).is_some()) {
                let parsed_end = parse_script_number_ui(end_digits).ok_or(())?;
                if parsed_end < start_number || parsed_end.saturating_sub(start_number) > 31 {
                    return Err(());
                }
                end_number = parsed_end;
                suffix = trailing;
            } else if has_trailing_sep
                && let Some(parsed_end) = parse_script_number_ui(end_digits)
                && parsed_end >= start_number
                && parsed_end.saturating_sub(start_number) <= 31
            {
                end_number = parsed_end;
                suffix = trailing;
            }
        }
    }

    if is_arabic_kw && is_markdown && suffix.is_empty() {
        return Ok(Some((start_number, end_number)));
    }
    let Some(raw_title) = strip_title_sep_ui(suffix) else {
        return if is_markdown { Err(()) } else { Ok(None) };
    };
    let title = raw_title.trim();
    if title.is_empty() || title.chars().count() > 1536 {
        return Err(());
    }
    Ok(Some((start_number, end_number)))
}

/// A display estimate only: runtime still validates all authored PAGE blocks and fields.
pub(super) fn explicit_script_pages(brief: &str) -> Option<u32> {
    if brief.len() > designcraft_ai_types::protocol::MAX_BRIEF_BYTES || brief.contains('\0') {
        return None;
    }
    let mut pages = 0u32;
    let mut fenced = false;
    for line in brief.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        match parse_script_heading_ui(trimmed) {
            Ok(Some((start_number, end_number))) => {
                if start_number != pages.checked_add(1)? || end_number > 1000 {
                    return None;
                }
                pages = end_number;
            }
            Ok(None) => {}
            Err(()) => return None,
        }
    }
    (pages > 0).then_some(pages)
}

pub(crate) fn is_retired_gemini_text_model(provider: ProviderKind, model: &str) -> bool {
    designcraft_ai_types::is_retired_gemini_model(provider, model)
}

pub(super) fn is_planner_prompt_ui(lower: &str) -> bool {
    [
        "planner",
        "journal",
        "notebook",
        "diary",
        "agenda",
        "habit tracker",
        "gratitude",
        "cornell",
        "daily productivity",
        "bullet journal",
        "بلانر",
        "مفكرة",
        "مفكره",
        "أجندة",
        "اجندة",
        "دفتر يوميات",
        "دفتر تخطيط",
        "منظم يومي",
        "منظم سنوي",
        "تتبع العادات",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
}

#[derive(Clone)]
pub(super) struct PromptAnalysisCache {
    pub(super) len: usize,
    pub(super) hash: u64,
    pub(super) explicit_pages: Option<u32>,
    pub(super) natural_pages: Option<u32>,
    pub(super) has_rtl: bool,
    pub(super) auto_binding_false: bool,
    pub(super) normalized_search: String,
    pub(super) is_planner: bool,
    pub(super) preview_head: String,
}

pub(super) static EMPTY_PROMPT_CACHE: PromptAnalysisCache = PromptAnalysisCache {
    len: 0,
    hash: 0,
    explicit_pages: None,
    natural_pages: None,
    has_rtl: false,
    auto_binding_false: false,
    normalized_search: String::new(),
    is_planner: false,
    preview_head: String::new(),
};

impl PromptAnalysisCache {
    pub(super) fn fingerprint(prompt: &str) -> (usize, u64) {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        prompt.hash(&mut hasher);
        (prompt.len(), hasher.finish())
    }

    pub(super) fn build(prompt: &str, len: usize, hash: u64) -> Self {
        let explicit_pages = explicit_script_pages(prompt);
        let natural_pages = if explicit_pages.is_none() { natural_prompt_pages(prompt) } else { None };
        let sample = if prompt.len() > 8_192 {
            let mut end = 8_192;
            while end > 0 && !prompt.is_char_boundary(end) {
                end -= 1;
            }
            prompt.get(..end).unwrap_or(prompt)
        } else {
            prompt
        };
        let has_rtl = rtl::has_rtl(sample);
        let auto_binding_false = automatic_proof_binding(sample, false);
        let normalized_search = normalize_skill_search_ui(sample);
        let is_planner = is_planner_prompt_ui(&normalized_search);
        let preview_head = prompt
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .take(3)
            .map(|line| {
                let mut out: String = line.chars().take(120).collect();
                if line.chars().count() > 120 {
                    out.push('…');
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n");
        Self { len, hash, explicit_pages, natural_pages, has_rtl, auto_binding_false, normalized_search, is_planner, preview_head }
    }
}

/// Search-only normalization matching runtime style keyword matching; prompt text stays exact.
pub(super) fn normalize_skill_search_ui(text: &str) -> String {
    let stripped = text
        .chars()
        .filter(|c| !matches!(*c, '\u{0610}'..='\u{061a}' | '\u{0640}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06ed}'))
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let mut normalized = String::with_capacity(stripped.len().saturating_add(8));
    let mut prev_arabic = None;
    for ch in stripped.chars() {
        let is_arabic = ('\u{0600}'..='\u{06ff}').contains(&ch);
        let is_latin_or_digit = ch.is_ascii_alphanumeric();
        if let Some(was_arabic) = prev_arabic
            && ((was_arabic && is_latin_or_digit) || (!was_arabic && is_arabic))
            && !normalized.ends_with(' ')
        {
            normalized.push(' ');
        }
        normalized.push(ch);
        prev_arabic = if is_arabic {
            Some(true)
        } else if is_latin_or_digit {
            Some(false)
        } else {
            None
        };
    }
    normalized
}

/// Conservative natural-language total-page hint used only when no explicit `# PAGE N` script is authored.
pub(super) fn natural_prompt_pages(prompt: &str) -> Option<u32> {
    if prompt.len() > designcraft_ai_types::protocol::MAX_BRIEF_BYTES || prompt.contains('\0') {
        return None;
    }
    let normalized: String = prompt
        .chars()
        .filter(|ch| !matches!(*ch, '\u{0610}'..='\u{061a}' | '\u{0640}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06ed}'))
        .map(|ch| match ch {
            '\u{0660}'..='\u{0669}' => char::from_u32(u32::from('0').saturating_add(u32::from(ch).saturating_sub(0x0660))).unwrap_or(ch),
            '\u{06F0}'..='\u{06F9}' => char::from_u32(u32::from('0').saturating_add(u32::from(ch).saturating_sub(0x06F0))).unwrap_or(ch),
            _ => ch,
        })
        .collect();

    let mut fenced = false;
    let mut inline_candidate: Option<u32> = None;
    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_lowercase();
        for header in
            ["بعدد الصفحات", "عدد صفحات الكتاب", "عدد الصفحات", "إجمالي الصفحات", "اجمالي الصفحات", "page count", "total pages", "number of pages"]
        {
            if let Some(idx) = lower.find(header)
                && let Some(after_header) = lower.get(idx.saturating_add(header.len())..)
            {
                let rest = after_header.trim_start_matches([' ', '\t', ':', '=', '-', '–', '—', '(', '[', '«']);
                let end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
                let next_char = rest.get(end..).and_then(|s| s.chars().next());
                if end > 0
                    && next_char != Some('.')
                    && let Some(n) = rest.get(..end).and_then(|digits| digits.parse::<u32>().ok())
                    && (2..=1000).contains(&n)
                {
                    return Some(n);
                }
            }
        }
        if inline_candidate.is_some() {
            continue;
        }
        let mut cursor = 0usize;
        while let Some(slice) = lower.get(cursor..) {
            let Some(rel_start) = slice.find(|c: char| c.is_ascii_digit()) else {
                break;
            };
            let start = cursor.saturating_add(rel_start);
            let Some(from_digits) = lower.get(start..) else {
                break;
            };
            let len = from_digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(from_digits.len());
            let end = start.saturating_add(len);
            cursor = end.max(cursor.saturating_add(1));

            let before = lower.get(..start).unwrap_or("");
            let prev_char = before.chars().next_back();
            if prev_char.is_some_and(|c| c == '.' || c == '/' || c == '#' || c.is_ascii_alphabetic() || ('\u{0600}'..='\u{06ff}').contains(&c)) {
                continue;
            }
            let before_unbracketed = before.trim_end_matches([' ', '\t', '(', '[', '«']);
            if ["صفحة", "صفحه", "الصفحة", "الصفحه", "صفحات", "page", "pages"].iter().any(|p| before_unbracketed.ends_with(p))
            {
                continue;
            }
            if before_unbracketed.ends_with(['-', '–', '—'])
                && before_unbracketed.trim_end_matches(['-', '–', '—', ' ', '\t']).chars().next_back().is_some_and(|c| c.is_ascii_digit())
            {
                continue;
            }
            let Some(number) = lower.get(start..end).and_then(|d| d.parse::<u32>().ok()).filter(|n| (2..=1000).contains(n)) else {
                continue;
            };
            let Some(after) = lower.get(end..) else {
                continue;
            };
            if after.starts_with('.') && after.get(1..).and_then(|s| s.chars().next()).is_some_and(|c| c.is_ascii_digit()) {
                continue;
            }
            let after_unbracketed = after.trim_start_matches([')', ']', '»']);
            if let Some(hyphen_tail) = after_unbracketed.strip_prefix("-pages").or_else(|| after_unbracketed.strip_prefix("-page"))
                && hyphen_tail.chars().next().is_none_or(|c| !c.is_alphabetic() && c != '_')
            {
                let tail_trimmed = hyphen_tail.trim_start_matches([' ', '\t', '-', '–', '—']);
                if !["لكل", "في كل", "per ", "per\t"].iter().any(|p| tail_trimmed.starts_with(p)) {
                    inline_candidate = Some(number);
                    break;
                }
                continue;
            }
            let unit_rest = after_unbracketed.trim_start_matches([' ', '\t']);
            for unit in ["صفحات", "صفحة", "صفحه", "pages", "page"] {
                if let Some(tail) = unit_rest.strip_prefix(unit)
                    && tail.chars().next().is_none_or(|c| !c.is_alphabetic() && c != '_')
                {
                    let tail_trimmed = tail.trim_start_matches([' ', '\t', '-', '–', '—']);
                    let per_group =
                        ["لكل", "في كل", "بالدفعة", "في الدفعة", "في المراجعة", "per ", "per\t"].iter().any(|p| tail_trimmed.starts_with(p));
                    if !per_group {
                        inline_candidate = Some(number);
                        break;
                    }
                }
            }
            if inline_candidate.is_some() {
                break;
            }
        }
    }
    if inline_candidate.is_none() {
        let lower_all = normalized.to_lowercase();
        if is_planner_prompt_ui(&lower_all) && ["365", "سنوي", "سنوية", "annual", "yearly", "year-round"].iter().any(|kw| lower_all.contains(kw))
        {
            return Some(365);
        }
    }
    inline_candidate
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Human review belongs to one complete, immutable proof manifest and document revision.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct BatchReviewKey {
    pub(super) job_id: String,
    pub(super) checkpoint: u32,
    pub(super) batch: u32,
    pub(super) document: (u64, u64),
    pub(super) page_range: (u32, u32),
    pub(super) task_shape: (u32, u32, u32),
    pub(super) proofs: Vec<(String, String)>,
}
impl BatchReviewKey {
    pub(super) fn from_batch(batch: &BatchSnapshot) -> Option<Self> {
        if batch.status != "awaiting_human_proof" {
            return None;
        }
        let display = ProofDisplay::from_batch(batch)?;
        Some(Self {
            job_id: batch.job_id.clone(),
            checkpoint: batch.checkpoint,
            batch: batch.current_batch,
            document: (batch.document_uid?, batch.document_revision?),
            page_range: display.page_range,
            task_shape: (batch.total_pages, batch.pages_per_batch, batch.total_batches),
            proofs: display.proofs,
        })
    }

    pub(super) fn pages(&self) -> impl Iterator<Item = &(String, String)> {
        self.proofs.iter().filter(|(path, _)| path.to_ascii_lowercase().ends_with(".png"))
    }

    pub(super) fn same_proof_except_revision(&self, other: &Self) -> bool {
        let mut normalized = other.clone();
        normalized.document.1 = self.document.1;
        self == &normalized
    }
}

/// A saved proof is display data, never authority to approve or resume a worker.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct ProofDisplay {
    pub(super) job_id: String,
    pub(super) batch: u32,
    pub(super) page_range: (u32, u32),
    pub(super) proofs: Vec<(String, String)>,
}
impl ProofDisplay {
    pub(super) fn from_batch(batch: &BatchSnapshot) -> Option<Self> {
        let mut reports = batch.batches.iter().filter(|report| report.batch == batch.current_batch);
        let current = reports.next()?;
        if reports.next().is_some() {
            return None;
        }
        let max_proofs =
            usize::try_from(current.page_count).ok()?.checked_add(usize::try_from(current.page_count.div_ceil(32)).ok()?)?.checked_add(1)?;
        if current.proof_paths.len() != current.proof_sha256.len()
            || current.proof_paths.len() > max_proofs
            || !(1..=1000).contains(&current.page_count)
        {
            return None;
        }
        let mut pages = BTreeSet::new();
        let mut paths = BTreeSet::new();
        for (path, sha) in current.proof_paths.iter().zip(&current.proof_sha256) {
            if path.is_empty()
                || !paths.insert(path.replace('\\', "/").to_ascii_lowercase())
                || sha.len() != 64
                || !sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return None;
            }
            if path.to_ascii_lowercase().ends_with(".png") && !pages.insert(path) {
                return None;
            }
        }
        if u32::try_from(pages.len()).ok() != Some(current.page_count) {
            return None;
        }
        Some(Self {
            job_id: batch.job_id.clone(),
            batch: batch.current_batch,
            page_range: (current.page_start, current.page_count),
            proofs: current.proof_paths.iter().cloned().zip(current.proof_sha256.iter().cloned()).collect(),
        })
    }

    pub(super) fn pages(&self) -> impl Iterator<Item = &(String, String)> {
        self.proofs.iter().filter(|(path, _)| path.to_ascii_lowercase().ends_with(".png"))
    }
}

#[derive(Clone)]
pub struct AiUiState {
    pub open: bool,
    pub(crate) preview_update: crate::preview_update::PreviewUpdate,
    pub tab: usize,
    pub snapshot: AiSnapshot,
    pub settings: AiSettings,
    pub(super) initialized: bool,
    pub(super) last_poll: Option<Instant>,
    pub(super) api_key: Secret,
    pub(super) key_for_images: bool,
    pub(super) prompt: String,
    pub(super) new_document: bool,
    pub(super) continue_book: bool,
    pub(super) restored_target_unconfirmed: bool,
    pub(super) pages: u32,
    pub(super) automatic_skills: bool,
    pub(super) selected_skills: BTreeSet<String>,
    pub(super) directory: String,
    pub(super) import_path: String,
    pub(super) export_path: String,
    pub(super) skill_export_path: String,
    pub(super) skill_prompt: String,
    pub(super) build_design_style: bool,
    pub(super) correction_before: String,
    pub(super) correction_after: String,
    pub(super) correction_reason: String,
    pub(super) references: String,
    pub(super) review_key: Option<(String, String)>,
    pub(super) skill_md: String,
    pub(super) manifest_json: String,
    pub(super) preview_path: Option<String>,
    pub(super) preview: Option<egui::TextureHandle>,
    pub(super) preview_error: String,
    pub(super) error: String,
    pub(super) printer: String,
    pub(super) grayscale: bool,
    pub(super) duplex: String,
    pub(super) rates_confirmed: bool,
    pub(super) workflow_json: String,
    pub(super) workflow_error: String,
    pub(super) proof_key: Option<BatchReviewKey>,
    pub(super) proof_display: Option<ProofDisplay>,
    pub(super) show_saved_proof: bool,
    pub(super) reviewed_proofs: BTreeSet<(String, String)>,
    pub(super) proof_document_version: Option<(u64, u64)>,
    pub(super) proof_validation_requested: Option<(u64, u64)>,
    pub(super) proof_document_matches: bool,
    pub(super) proof_facing_pages: Option<bool>,
    pub(super) proof_binding: Option<bool>,
    pub(super) proof_pair_view: bool,
    pub(super) proof_full_resolution: bool,
    pub(super) review_input_is_human: bool,
    pub(super) review_pointer_press: Option<(BatchReviewKey, egui::Id)>,
    pub(super) batch_approval_gesture: Option<(BatchReviewKey, egui::Context, u64)>,
    pub(super) proof_path: Option<String>,
    pub(super) proof_loaded: Option<String>,
    pub(super) proof_texture: Option<egui::TextureHandle>,
    pub(super) companion_proof: Option<CompanionProof>,
    pub(super) proof_error: String,
    pub(super) local_ollama: ModelSelection,
    pub(super) local_comfy: ModelSelection,
    pub(super) pending_models: Option<ModelSelection>,
    pub(super) local_probe: Option<(ModelSelection, Vec<String>, String, bool)>,
    pub(super) discovered_models: BTreeMap<(ProviderKind, Option<String>), Vec<String>>,
    pub(super) print_token: Option<String>,
    pub(super) print_reviewed: bool,
    pub(super) print_save_path: String,
    pub(super) ollama_executable: String,
    pub(super) pick_ollama: bool,
    pub(super) open_document_path: String,
    pub(super) pick_target_document: bool,
    pub(super) open_target_document_requested: bool,
    pub(super) pick_script_file: bool,
    pub(super) edit_large_script_inline: bool,
    pub(super) prompt_cache: std::cell::RefCell<Option<PromptAnalysisCache>>,
    pub(super) expand_advanced_settings: Option<bool>,
}
impl std::fmt::Debug for AiUiState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiUiState")
            .field("open", &self.open)
            .field("tab", &self.tab)
            .field("busy", &self.snapshot.busy)
            .field("api_key", &self.api_key)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
impl AiUiState {
    pub(crate) fn prompt_text(&self) -> &str {
        &self.prompt
    }
}

impl Default for AiUiState {
    fn default() -> Self {
        Self {
            open: false,
            preview_update: crate::preview_update::PreviewUpdate::default(),
            tab: 0,
            snapshot: AiSnapshot::default(),
            settings: AiSettings { execution_pacing: ExecutionPacing::AutoContinue, ..Default::default() },
            initialized: false,
            last_poll: None,
            api_key: Secret::default(),
            key_for_images: false,
            prompt: String::new(),
            new_document: true,
            continue_book: false,
            restored_target_unconfirmed: false,
            pages: 8,
            automatic_skills: true,
            selected_skills: BTreeSet::new(),
            directory: String::new(),
            import_path: String::new(),
            export_path: String::new(),
            skill_export_path: "skill-package.zip".into(),
            skill_prompt: String::new(),
            build_design_style: true,
            correction_before: String::new(),
            correction_after: String::new(),
            correction_reason: String::new(),
            references: String::new(),
            review_key: None,
            skill_md: String::new(),
            manifest_json: String::new(),
            preview_path: None,
            preview: None,
            preview_error: String::new(),
            error: String::new(),
            printer: String::new(),
            grayscale: false,
            duplex: "off".into(),
            rates_confirmed: false,
            workflow_json: String::new(),
            workflow_error: String::new(),
            proof_key: None,
            proof_display: None,
            show_saved_proof: true,
            reviewed_proofs: BTreeSet::new(),
            proof_document_version: None,
            proof_validation_requested: None,
            proof_document_matches: false,
            proof_facing_pages: None,
            proof_binding: None,
            proof_pair_view: true,
            proof_full_resolution: false,
            review_input_is_human: false,
            review_pointer_press: None,
            batch_approval_gesture: None,
            proof_path: None,
            proof_loaded: None,
            proof_texture: None,
            companion_proof: None,
            proof_error: String::new(),
            local_ollama: ModelSelection { provider: ProviderKind::Ollama, model: String::new(), endpoint: Some("http://127.0.0.1:11434".into()) },
            local_comfy: ModelSelection { provider: ProviderKind::ComfyUi, model: String::new(), endpoint: Some("http://127.0.0.1:8188".into()) },
            pending_models: None,
            local_probe: None,
            discovered_models: BTreeMap::new(),
            print_token: None,
            print_reviewed: false,
            print_save_path: String::new(),
            ollama_executable: String::new(),
            pick_ollama: false,
            open_document_path: String::new(),
            pick_target_document: false,
            open_target_document_requested: false,
            pick_script_file: false,
            edit_large_script_inline: false,
            prompt_cache: std::cell::RefCell::new(None),
            expand_advanced_settings: None,
        }
    }
}

pub(super) fn rate_table_configured(rates: &TokenRates) -> bool {
    rates.input_per_million > 0.0
        || rates.output_per_million > 0.0
        || rates.image_per_request.is_some_and(|v| v > 0.0)
        || rates.image_reserve_usd.is_some_and(|v| v > 0.0)
}

pub(super) fn has_configured_rates(settings: &AiSettings) -> bool {
    let text_ok = settings.budget.rates.as_ref().is_some_and(|rates| rates.input_per_million > 0.0 || rates.output_per_million > 0.0);
    if settings.output_mode == OutputMode::NativeWithImages && settings.visual_density != VisualDensityMode::PureNativeVector {
        let image_rates = settings.image_rates.as_ref().or(settings.budget.rates.as_ref());
        text_ok && image_rates.is_some_and(rate_table_configured)
    } else {
        text_ok
    }
}

pub(super) fn is_skill_scoped_error(msg: &str) -> bool {
    let trimmed = msg.trim();
    trimmed.starts_with("skill ")
        || trimmed.starts_with("Skill ")
        || trimmed.starts_with("Unsupported skill")
        || trimmed.starts_with("Run and visually review")
}

impl AiUiState {
    pub(super) fn analyzed_prompt(&self) -> std::cell::Ref<'_, PromptAnalysisCache> {
        let (len, hash) = PromptAnalysisCache::fingerprint(&self.prompt);
        let needs_rebuild = self.prompt_cache.borrow().as_ref().is_none_or(|cached| cached.len != len || cached.hash != hash);
        if needs_rebuild {
            let built = PromptAnalysisCache::build(&self.prompt, len, hash);
            *self.prompt_cache.borrow_mut() = Some(built);
        }
        std::cell::Ref::map(self.prompt_cache.borrow(), |opt| match opt {
            Some(cached) => cached,
            None => &EMPTY_PROMPT_CACHE,
        })
    }

    pub(super) fn requested_target_pages(&self) -> Option<u32> {
        let analysis = self.analyzed_prompt();
        analysis.explicit_pages.or(analysis.natural_pages).or_else(|| {
            self.snapshot.production_draft.as_ref().and_then(|draft| draft.progress.target_total_pages).filter(|pages| (1..=1000).contains(pages))
        })
    }

    pub(super) fn load_production_request(&mut self, request: &TaskRequest) {
        self.prompt = request.prompt.clone();
        self.edit_large_script_inline = false;
        self.settings = request.settings.clone();
        self.automatic_skills = request.auto_select_skills;
        self.selected_skills = request.skill_ids.iter().cloned().collect();
        self.directory = request.export_directory.clone().unwrap_or_default();
        self.new_document = matches!(request.target, TaskTarget::NewA4 { .. });
        self.continue_book = matches!(request.target, TaskTarget::ContinueBook);
        if let TaskTarget::NewA4 { pages } = request.target {
            self.pages = pages;
        }
        self.rates_confirmed = has_configured_rates(&self.settings);
        self.workflow_json = self.settings.comfy_workflow.as_ref().map(Value::to_string).unwrap_or_default();
        self.workflow_error.clear();
        self.restored_target_unconfirmed = false;
    }

    pub(super) fn sync_settings(&mut self, settings: AiSettings) {
        self.settings = settings;
        // Reuse explicitly configured prices; restarting does not require the same setup again.
        self.rates_confirmed = has_configured_rates(&self.settings);
        self.workflow_json = self.settings.comfy_workflow.as_ref().map(Value::to_string).unwrap_or_default();
        self.workflow_error.clear();
        if self.settings.text_model.provider == ProviderKind::Ollama {
            self.local_ollama = self.settings.text_model.clone();
        }
        if self.settings.image_model.provider == ProviderKind::ComfyUi {
            self.local_comfy = self.settings.image_model.clone();
        }
    }

    pub(super) fn accept(&mut self, snapshot: AiSnapshot) {
        if !self.initialized && snapshot.available {
            self.sync_settings(snapshot.settings.clone());
            if let Some(draft) = &snapshot.production_draft {
                let preserve_saved_settings = !snapshot.busy
                    && snapshot.batch.is_none()
                    && serde_json::to_value(&snapshot.settings).ok() != serde_json::to_value(AiSettings::default()).ok();
                self.load_production_request(&draft.request);
                if preserve_saved_settings {
                    let mut merged = draft.request.settings.clone();
                    merged.text_model = snapshot.settings.text_model.clone();
                    merged.image_model = snapshot.settings.image_model.clone();
                    merged.budget = snapshot.settings.budget.clone();
                    merged.image_rates = snapshot.settings.image_rates.clone();
                    merged.resources = snapshot.settings.resources.clone();
                    merged.comfy_workflow = snapshot.settings.comfy_workflow.clone();
                    merged.execution_pacing = snapshot.settings.execution_pacing;
                    merged.visual_density = snapshot.settings.visual_density;
                    if snapshot.settings.arabic_tashkeel != ArabicTashkeelMode::Auto {
                        merged.arabic_tashkeel = snapshot.settings.arabic_tashkeel;
                    }
                    if snapshot.settings.arabic_font.is_some() {
                        merged.arabic_font = snapshot.settings.arabic_font.clone();
                    }
                    self.sync_settings(merged);
                }
                if let Some(pages) = draft.progress.target_total_pages {
                    self.pages = pages;
                }
                self.restored_target_unconfirmed = !self.new_document;
            }
            self.initialized = true;
        }
        let key = snapshot.review.as_ref().map(|r| (r.id.clone(), r.revision.clone()));
        if key != self.review_key {
            if let Some(r) = &snapshot.review {
                self.skill_md = r.skill_md.clone();
                self.manifest_json = r.manifest_json.clone();
            }
            self.review_key = key;
        }
        if self.snapshot.review.as_ref().and_then(|r| r.sample_path.as_ref()) != snapshot.review.as_ref().and_then(|r| r.sample_path.as_ref()) {
            self.preview_path = None;
            self.preview = None;
        }
        let proof_key = snapshot.batch.as_ref().and_then(BatchReviewKey::from_batch);
        let proof_display = snapshot.batch.as_ref().and_then(ProofDisplay::from_batch);
        if self.proof_key != proof_key || self.proof_display != proof_display {
            self.show_saved_proof = true;
            let preserve_review = self
                .proof_key
                .as_ref()
                .zip(proof_key.as_ref())
                .is_some_and(|(previous, next)| self.proof_document_version == Some(next.document) && previous.same_proof_except_revision(next));
            self.review_pointer_press = None;
            self.batch_approval_gesture = None;
            self.proof_validation_requested = None;
            if !preserve_review {
                self.proof_path = proof_display.as_ref().and_then(|key| key.pages().next()).map(|(path, _)| path.clone());
                self.reviewed_proofs.clear();
                self.proof_facing_pages = None;
                self.proof_binding = None;
                self.proof_loaded = None;
                self.proof_texture = None;
                self.companion_proof = None;
                self.proof_error.clear();
            }
            self.proof_document_matches = false;
            self.proof_key = proof_key;
            self.proof_display = proof_display;
        }
        if !snapshot.busy
            && let Some(selection) = self.pending_models.take()
        {
            let success = snapshot.stage == "completed";
            let key = (selection.provider, selection.endpoint.clone());
            if success && !snapshot.models.is_empty() {
                self.discovered_models.insert(key, snapshot.models.clone());
            } else {
                self.discovered_models.remove(&key);
            }
            self.local_probe = Some((selection, if success { snapshot.models.clone() } else { Vec::new() }, snapshot.message.clone(), success));
        }
        let print_token = snapshot.print_proof.as_ref().map(|proof| proof.token.clone());
        if print_token != self.print_token {
            self.print_token = print_token;
            self.print_reviewed = false;
        }
        self.snapshot = snapshot;
    }

    pub(super) fn sync_proof_document(&mut self, document: Option<(u64, u64)>) {
        if self.proof_document_version != document {
            if self.proof_document_version.map(|(uid, _)| uid) != document.map(|(uid, _)| uid) {
                self.reviewed_proofs.clear();
            }
            self.proof_document_version = document;
        }
        self.proof_document_matches = self.proof_key.as_ref().is_some_and(|key| Some(key.document) == document);
        if !self.proof_document_matches {
            self.proof_facing_pages = None;
            self.proof_binding = None;
        }
    }

    pub(super) fn effective_proof_facing_pages(&self) -> bool {
        self.proof_facing_pages.unwrap_or(false) || self.settings.facing_pages
    }

    pub(super) fn can_review_selected_proof(&self) -> bool {
        self.review_input_is_human
            && !self.snapshot.busy
            && self.proof_document_matches
            && self.proof_texture.is_some()
            && self.proof_error.is_empty()
            && self.proof_loaded == self.proof_path
            && self.proof_path.as_ref().is_some_and(|path| self.proof_key.as_ref().is_some_and(|key| key.pages().any(|(page, _)| page == path)))
    }

    /// Only an explicit per-page UI gesture records review; preview loading never does.
    pub(super) fn review_selected_proof(&mut self) {
        if !self.can_review_selected_proof() {
            return;
        }
        if let Some(page) = self.proof_key.as_ref().and_then(|key| key.pages().find(|(path, _)| self.proof_path.as_deref() == Some(path.as_str()))) {
            self.reviewed_proofs.insert(page.clone());
        }
    }

    /// Navigation never carries a loaded texture or a held review gesture to another page.
    pub(super) fn select_proof(&mut self, path: &str) -> bool {
        if self.proof_path.as_deref() == Some(path) || !self.proof_display.as_ref().is_some_and(|key| key.pages().any(|(page, _)| page == path)) {
            return false;
        }
        self.proof_path = Some(path.into());
        self.proof_loaded = None;
        self.proof_texture = None;
        self.proof_error.clear();
        self.review_pointer_press = None;
        true
    }

    pub(super) fn companion_proof_path(&self) -> Option<&str> {
        if !self.effective_proof_facing_pages() || !self.proof_pair_view {
            return None;
        }
        let key = self.proof_display.as_ref()?;
        let selected = key.pages().position(|(path, _)| self.proof_path.as_deref() == Some(path.as_str()))?;
        let page = key.page_range.0.checked_add(u32::try_from(selected).ok()?)?;
        let rtl = self.proof_binding.or(self.settings.right_to_left_binding).unwrap_or_else(|| self.analyzed_prompt().auto_binding_false);
        let pair = facing_proof_pair(page, rtl)?;
        let companion = pair.into_iter().find(|candidate| *candidate != page)?;
        let index = companion.checked_sub(key.page_range.0)?;
        key.pages().nth(usize::try_from(index).ok()?).map(|(path, _)| path.as_str())
    }

    pub(super) fn can_review_pair(&self) -> bool {
        self.proof_full_resolution
            && self.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "awaiting_human_proof")
            && self.can_review_selected_proof()
            && self.companion_proof_path().is_some_and(|path| {
                self.companion_proof.as_ref().is_some_and(|proof| proof.path == path && proof.texture.is_some() && proof.error.is_empty())
            })
    }

    pub(super) fn production_task(&self) -> TaskRequest {
        TaskRequest {
            prompt: self.prompt.clone(),
            settings: self.settings.clone(),
            auto_select_skills: self.automatic_skills,
            skill_ids: self.selected_skills.iter().cloned().collect(),
            target: if self.new_document {
                TaskTarget::NewA4 { pages: self.pages }
            } else if self.continue_book {
                TaskTarget::ContinueBook
            } else {
                TaskTarget::CurrentDocument
            },
            export_directory: (!self.directory.trim().is_empty()).then(|| self.directory.clone()),
        }
    }

    pub(super) fn review_displayed_pair(&mut self) {
        if !self.can_review_pair() {
            return;
        }
        let companion = self.companion_proof_path().map(str::to_owned);
        self.review_selected_proof();
        if let Some(page) = self.proof_key.as_ref().and_then(|key| key.pages().find(|(path, _)| Some(path.as_str()) == companion.as_deref())) {
            self.reviewed_proofs.insert(page.clone());
        }
        let next =
            self.proof_key.as_ref().and_then(|key| key.pages().find(|page| !self.reviewed_proofs.contains(*page))).map(|(path, _)| path.clone());
        if let Some(next) = next {
            self.select_proof(&next);
        }
    }

    pub(super) fn can_approve_batch(&self) -> bool {
        self.review_input_is_human
            && !self.snapshot.busy
            && self.proof_document_matches
            && self.proof_error.is_empty()
            && self.proof_key.as_ref().is_some_and(|key| key.pages().all(|page| self.reviewed_proofs.contains(page)))
    }

    /// A clicked response alone may come from accessibility or stale widget state.
    /// Bind a primary press/release (or non-repeated focused key press) to this proof.
    pub(super) fn fresh_review_gesture(
        human: bool,
        key: Option<&BatchReviewKey>,
        pointer_press: &mut Option<(BatchReviewKey, egui::Id)>,
        response: &egui::Response,
    ) -> bool {
        if !human {
            *pointer_press = None;
            return false;
        }
        let Some(key) = key else { return false };
        let pointer_down = response.is_pointer_button_down_on();
        let pointer_clicked = response.clicked_by(egui::PointerButton::Primary);
        let keyboard_clicked = response.clicked() && response.has_focus();
        response.ctx.input(|input| {
            let mut pointer = false;
            for event in &input.events {
                if let egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, .. } = event {
                    if *pressed {
                        // A coalesced press+release has no held-button state at frame end.
                        // Require egui to attribute the interaction to this widget as well.
                        if response.rect.contains(*pos) && (pointer_down || pointer_clicked) {
                            *pointer_press = Some((key.clone(), response.id));
                        } else if pointer_press.as_ref().is_some_and(|(_, id)| *id == response.id) {
                            *pointer_press = None;
                        }
                    } else if pointer_press.as_ref().is_some_and(|(_, id)| *id == response.id) {
                        pointer = pointer_clicked
                            && response.rect.contains(*pos)
                            && pointer_press.as_ref().is_some_and(|(pressed_key, _)| pressed_key == key);
                        *pointer_press = None;
                    }
                }
            }
            let keyboard = keyboard_clicked
                && input
                    .events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Key { key: egui::Key::Enter | egui::Key::Space, pressed: true, repeat: false, .. }));
            pointer || keyboard
        })
    }

    pub(super) fn task_active(&self) -> bool {
        self.snapshot.busy || self.snapshot.batch.as_ref().is_some_and(|b| matches!(b.status.as_str(), "running" | "awaiting_human_proof"))
    }

    /// Parse only after an edit. The raw draft survives partial JSON, repaints and host polls.
    pub(super) fn edit_workflow(&mut self) {
        match serde_json::from_str(&self.workflow_json) {
            Ok(value) => {
                self.settings.comfy_workflow = Some(value);
                self.workflow_error.clear();
            }
            Err(error) => {
                self.settings.comfy_workflow = None;
                self.workflow_error = error.to_string();
            }
        }
    }

    pub(super) fn workflow_ready(&self) -> bool {
        self.settings.output_mode != OutputMode::NativeWithImages
            || self.settings.visual_density == VisualDensityMode::PureNativeVector
            || self.settings.image_model.provider != ProviderKind::ComfyUi
            || (self.workflow_error.is_empty() && self.settings.comfy_workflow.is_some())
    }

    pub(super) fn production_issue(&self, available: bool, document: bool, arabic: bool) -> Option<&'static str> {
        let uses_images =
            self.settings.output_mode == OutputMode::NativeWithImages && self.settings.visual_density != VisualDensityMode::PureNativeVector;
        let (en, ar) = if !available {
            ("Native AI host is unavailable. Open the desktop application.", "خدمة الذكاء غير متاحة. افتح تطبيق سطح المكتب.")
        } else if self.snapshot.busy {
            ("A task is running. Wait for it to finish or cancel it.", "توجد مهمة قيد التشغيل. انتظر انتهاءها أو ألغها.")
        } else if self.task_active() {
            (
                "Review and approve the current batch, or cancel the task before starting another.",
                "راجع الدفعة الحالية واعتمدها، أو ألغ المهمة قبل بدء مهمة أخرى.",
            )
        } else if self.prompt.trim().is_empty() {
            ("Enter production instructions.", "أدخل تعليمات الإنتاج.")
        } else if self.prompt.len() > designcraft_ai_types::protocol::MAX_BRIEF_BYTES {
            (
                "The full brief exceeds 1 MiB. Move large reference material into skill references.",
                "تجاوز نص التعليمات الكامل 1 ميبيبايت. انقل المراجع الكبيرة إلى مراجع المهارة.",
            )
        } else if self.settings.text_model.model.trim().is_empty() {
            ("Choose a text model above.", "اختر نموذج النص من الحقل أعلاه.")
        } else if is_non_text_model(&self.settings.text_model.model) {
            (
                "The selected text model is an image/media model. Choose a text model (for example gemini-3.8-flash) above.",
                "نموذج النص المختار مخصص للصور أو الوسائط. اختر نموذج نص (مثل gemini-3.8-flash) من الحقل أعلاه.",
            )
        } else if !self.new_document && !document {
            ("Open a document or choose New document.", "افتح مستنداً أو اختر مستنداً جديداً.")
        } else if self.restored_target_unconfirmed {
            (
                "Choose the target book or prepare its remaining pages before producing the saved request.",
                "اختر الكتاب المستهدف أو جهّز صفحاته المتبقية قبل إنتاج التعليمات المحفوظة.",
            )
        } else if uses_images && self.settings.image_model.provider == ProviderKind::Ollama {
            (
                "Ollama does not generate raster images here. Choose Gemini, OpenAI, or ComfyUI for the image model, or switch to Editable elements.",
                "مزود Ollama لا يولّد صوراً نقطية هنا. اختر Gemini أو OpenAI أو ComfyUI لنموذج الصور، أو حوّل الوضع إلى عناصر قابلة للتحرير.",
            )
        } else if uses_images && self.settings.image_model.provider != ProviderKind::ComfyUi && self.settings.image_model.model.trim().is_empty() {
            ("Choose an image model in Providers and keys.", "اختر نموذج الصور من تبويب المزودين والمفاتيح.")
        } else if uses_images
            && !self.settings.image_model.model.trim().is_empty()
            && is_non_image_model(self.settings.image_model.provider, &self.settings.image_model.model)
        {
            (
                "The selected image model is a text/non-image or unsupported endpoint model. Choose a supported image generation model (such as gemini-3.1-flash-image or gpt-image-1) or switch to Editable elements.",
                "نموذج الصور المختار مخصص للنصوص أو غير مدعوم على مسار التوليد الحالي. اختر نموذج توليد صور مدعوماً (مثل gemini-3.1-flash-image أو gpt-image-1) أو حوّل الوضع إلى عناصر قابلة للتحرير.",
            )
        } else if uses_images
            && self.settings.verify_generated_images
            && !supports_gemini_image_review(self.settings.text_model.provider, &self.settings.text_model.model)
        {
            (
                "Generated-image inspection requires a supported Gemini 2.5+ or 3.x text model (such as gemini-3.8-flash), or turn off image inspection in Advanced settings.",
                "فحص الصور المولدة يتطلب نموذج نص Gemini مدعوماً (مثل gemini-3.8-flash)، أو أوقف فحص الصور من الإعدادات المتقدمة.",
            )
        } else if !self.workflow_ready() {
            ("Complete a valid ComfyUI API workflow in Providers and keys.", "أكمل مخطط ComfyUI API صالحاً في تبويب المزودين والمفاتيح.")
        } else if self.settings.budget.max_cost_usd.is_some() && !self.rates_confirmed {
            ("Set and confirm provider prices in Advanced settings.", "أدخل أسعار المزود وأكّدها من الإعدادات المتقدمة.")
        } else {
            return None;
        };
        Some(text(arabic, en, ar))
    }

    pub(super) fn effective_design_profile(&self) -> Option<&designcraft_ai_types::DesignProfile> {
        self.effective_design_profile_with_id().map(|(_, profile)| profile)
    }

    pub(super) fn effective_design_profile_with_id(&self) -> Option<(&str, &designcraft_ai_types::DesignProfile)> {
        if let Some(id) = &self.settings.design_style {
            return self
                .snapshot
                .skills
                .iter()
                .rev()
                .filter(|card| &card.id == id && card.design_profile.is_some())
                .max_by_key(|card| u8::from(card.enabled && card.compatible) * 2 + u8::from(card.active))
                .and_then(|card| Some((card.id.as_str(), card.design_profile.as_ref()?)));
        }
        if !self.automatic_skills {
            return self
                .selected_skills
                .iter()
                .filter_map(|id| {
                    self.snapshot
                        .skills
                        .iter()
                        .rev()
                        .filter(|card| &card.id == id && card.enabled && card.compatible && card.design_profile.is_some())
                        .max_by_key(|card| u8::from(card.active))
                })
                .find_map(|card| Some((card.id.as_str(), card.design_profile.as_ref()?)));
        }
        if let Some(entry) = self
            .selected_skills
            .iter()
            .filter_map(|id| {
                self.snapshot
                    .skills
                    .iter()
                    .rev()
                    .find(|card| &card.id == id && card.enabled && card.compatible && card.active && card.design_profile.is_some())
            })
            .find_map(|card| Some((card.id.as_str(), card.design_profile.as_ref()?)))
        {
            return Some(entry);
        }
        let analysis = self.analyzed_prompt();
        let lower = analysis.normalized_search.as_str();
        if lower.trim().is_empty() {
            return None;
        }
        let planner_domain = analysis.is_planner;
        selectable_skill_cards(&self.snapshot.skills, true, false)
            .into_iter()
            .filter_map(|card| {
                let profile = card.design_profile.as_ref()?;
                let hits = profile
                    .keywords
                    .iter()
                    .map(|word| normalize_skill_search_ui(word))
                    .filter(|word| !word.is_empty() && lower.contains(word.as_str()))
                    .count();
                let kg_bonus = usize::from(hits > 0 && !planner_domain && card.id == "kg-oxford-cambridge-style") * 16;
                let planner_bonus = usize::from(planner_domain && card.id == "planner-journal-studio-style") * 24;
                let score = hits.saturating_add(kg_bonus).saturating_add(planner_bonus);
                (score > 0).then_some((score, card.id.as_str(), profile))
            })
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))
            .map(|(_, id, profile)| (id, profile))
    }

    pub(super) fn request_estimate(&self, documents: &[OpenDocument], active: Option<usize>) -> Option<RequestEstimate> {
        if (!self.new_document && !self.continue_book) || self.settings.pages_per_batch == 0 || self.prompt.trim().is_empty() {
            return None;
        }
        let (scripted_pages, is_planner) = {
            let analysis = self.analyzed_prompt();
            (analysis.explicit_pages, analysis.is_planner)
        };
        let requested_pages = self.requested_target_pages();
        let remaining = if self.new_document {
            if scripted_pages.is_some_and(|pages| pages != self.pages) {
                return None;
            }
            self.pages
        } else {
            let document = documents.iter().find(|document| Some(document.index) == active)?;
            if !document.can_continue_with_target(requested_pages, !self.prompt.trim().is_empty()) {
                return None;
            }
            if scripted_pages.is_some_and(|pages| {
                u32::try_from(document.pages).ok().is_none_or(|cur| cur > pages || (cur < pages && document.has_interior_gaps()))
            }) {
                return None;
            }
            document.effective_remaining_pages(requested_pages)?
        };
        let spread_aligned_new = self.new_document && self.settings.facing_pages && self.settings.pages_per_batch == 2 && remaining > 1;
        let review_pages = if spread_aligned_new { 1 } else { remaining.min(self.settings.pages_per_batch) };
        let profile_entry = self.effective_design_profile_with_id();
        let profile = profile_entry.map(|(_, p)| p);
        let effective_no_images =
            self.settings.output_mode == OutputMode::NativeOnly || self.settings.visual_density == VisualDensityMode::PureNativeVector;
        let is_planner_hybrid = !effective_no_images
            && self.settings.visual_density == VisualDensityMode::SmartHybrid
            && (profile_entry.is_some_and(|(id, _)| id == "planner-journal-studio-style") || (profile.is_none() && is_planner));
        let (image_allowance, book_images) = if effective_no_images {
            (0, 0)
        } else if is_planner_hybrid {
            let rev_imgs = review_pages.div_ceil(6).max(1);
            let bk_imgs = remaining.div_ceil(24).max(rev_imgs);
            (rev_imgs, bk_imgs)
        } else {
            let sources_per_page = profile
                .map(|p| p.page_families.iter().map(|family| family.image_slots.len()).max().unwrap_or(0))
                .map_or(1, |sources| u32::try_from(sources.clamp(1, 8)).unwrap_or(8));
            (review_pages.saturating_mul(sources_per_page), remaining.saturating_mul(sources_per_page))
        };
        let vision_allowance = if !effective_no_images && self.settings.verify_generated_images { image_allowance } else { 0 };
        let book_vision = if !effective_no_images && self.settings.verify_generated_images { book_images } else { 0 };
        let book_allowance = remaining.saturating_add(book_images).saturating_add(book_vision);
        (review_pages > 0).then_some(RequestEstimate {
            review_pages,
            remaining_pages: remaining,
            remaining_groups: if spread_aligned_new {
                1u32.saturating_add(remaining.saturating_sub(1).div_ceil(2))
            } else {
                remaining.div_ceil(self.settings.pages_per_batch)
            },
            book_allowance,
            scripted: scripted_pages.is_some(),
            image_allowance,
            vision_allowance,
        })
    }

    pub(super) fn token_estimate(&self, documents: &[OpenDocument], active: Option<usize>) -> Option<TokenEstimate> {
        let estimate = self.request_estimate(documents, active)?;
        let profile = self.effective_design_profile();
        let overhead_bytes: u64 = if estimate.scripted {
            let batch_share = u64::from(estimate.review_pages.clamp(1, 6));
            (if profile.is_some() { 48_000u64 } else { 56 * 1024 }) / batch_share
        } else if profile.is_some() {
            96_000
        } else if self.automatic_skills || !self.selected_skills.is_empty() {
            176 * 1024
        } else {
            48_000
        };
        let raw_prompt_bytes = u64::try_from(self.prompt.len()).unwrap_or(u64::MAX);
        let prompt_bytes = if estimate.scripted {
            (raw_prompt_bytes / u64::from(estimate.remaining_pages.max(1))).saturating_add(6_144).min(raw_prompt_bytes)
        } else {
            raw_prompt_bytes
        };
        let text_per_page = prompt_bytes
            .saturating_add(overhead_bytes)
            .div_ceil(3)
            .saturating_add(1024)
            .saturating_add(u64::from(self.settings.budget.max_output_tokens));
        let vision_per_page = if self.settings.output_mode == OutputMode::NativeWithImages
            && self.settings.visual_density != VisualDensityMode::PureNativeVector
            && estimate.image_allowance > 0
        {
            let workflow_tokens = self.settings.comfy_workflow.as_ref().map_or(0, |v| u64::try_from(v.to_string().len()).unwrap_or(0).div_ceil(3));
            let image_call_tokens = 2_048u64.saturating_add(1_024).saturating_add(workflow_tokens);
            let review_call_tokens = if self.settings.verify_generated_images {
                8_192u64.saturating_add(1_024).saturating_add(4_096).saturating_add(u64::from(self.settings.budget.max_output_tokens.min(4096)))
            } else {
                0
            };
            let per_image_tokens = image_call_tokens.saturating_add(review_call_tokens);
            u64::from(estimate.image_allowance).saturating_mul(per_image_tokens).div_ceil(u64::from(estimate.review_pages.max(1)))
        } else {
            0
        };
        let per_page_tokens = text_per_page.saturating_add(vision_per_page);
        Some(TokenEstimate {
            per_page_tokens,
            review_tokens: per_page_tokens.saturating_mul(u64::from(estimate.review_pages)).clamp(256, MAX_TOKEN_LIMIT),
            book_tokens: per_page_tokens.saturating_mul(u64::from(estimate.remaining_pages)).clamp(256, MAX_TOKEN_LIMIT),
        })
    }

    pub(super) fn live_phase(stage: &str, arabic: bool) -> &'static str {
        let (en, ar) = match stage {
            "planning" | "curriculum_plan" | "validating_skills" => ("Planning", "التخطيط"),
            "repair" => ("Repair", "الإصلاح"),
            "generating_images" => ("Image generation", "توليد الصور"),
            "executing" => ("Execution & preflight", "التنفيذ والفحص المطبعي"),
            "rendering_proof" => ("Proof preparation", "تجهيز البروفة"),
            "awaiting_human_proof" => ("Awaiting human proof", "بانتظار اعتماد البروفة"),
            "waiting_for_idle" => ("Waiting for canvas idle", "بانتظار انتهاء التفاعل لنشر النتيجة"),
            _ => ("Processing", "قيد المعالجة"),
        };
        text(arabic, en, ar)
    }

    pub(super) fn extract_message_page(message: &str, total_pages: u32) -> Option<(u32, u32)> {
        let lower = message.to_ascii_lowercase();
        for marker in ["pages ", "page "] {
            if let Some(idx) = lower.find(marker) {
                let rest = lower.get(idx + marker.len()..)?.trim_start();
                let end_first = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
                if let Some(first) = rest.get(..end_first).and_then(|s| s.parse::<u32>().ok())
                    && first >= 1
                    && first <= total_pages
                {
                    let after_first = rest.get(end_first..)?.trim_start();
                    if let Some(after_dash) = after_first.strip_prefix('–').or_else(|| after_first.strip_prefix('-')) {
                        let after_dash = after_dash.trim_start();
                        let end_second = after_dash.find(|c: char| !c.is_ascii_digit()).unwrap_or(after_dash.len());
                        if let Some(second) = after_dash.get(..end_second).and_then(|s| s.parse::<u32>().ok())
                            && second >= first
                            && second <= total_pages
                        {
                            return Some((first, second));
                        }
                    }
                    return Some((first, first));
                }
            }
        }
        None
    }

    pub(super) fn live_status_line(&self, arabic: bool) -> String {
        let phase = Self::live_phase(&self.snapshot.stage, arabic);
        let elapsed_ms = self.snapshot.batch.as_ref().map_or(0, |b| b.elapsed_ms);
        let timer = elapsed(elapsed_ms);
        if let Some(batch) = &self.snapshot.batch
            && batch.total_pages > 0
        {
            let batch_num = batch.current_batch.max(1);
            let total_batches = batch.total_batches.max(1);
            let (start_page, end_page) = Self::extract_message_page(&self.snapshot.message, batch.total_pages).unwrap_or_else(|| {
                if let Some(report) = batch.batches.iter().find(|r| r.batch == batch.current_batch)
                    && report.page_count > 0
                {
                    let start = report.page_start.saturating_add(1).min(batch.total_pages);
                    let end = report.page_start.saturating_add(report.page_count).min(batch.total_pages);
                    (start, end.max(start))
                } else {
                    let step = if batch.spread_aligned && batch.completed_pages == 0 { 1 } else { batch.pages_per_batch.max(1) };
                    let start = batch.completed_pages.saturating_add(1).min(batch.total_pages);
                    let end = batch.completed_pages.saturating_add(step).min(batch.total_pages);
                    (start, end.max(start))
                }
            });
            let page_part = if start_page == end_page {
                if arabic {
                    format!("الصفحة {start_page} / {}", batch.total_pages)
                } else {
                    format!("Page {start_page} / {}", batch.total_pages)
                }
            } else if arabic {
                format!("الصفحات {start_page}–{end_page} / {}", batch.total_pages)
            } else {
                format!("Pages {start_page}–{end_page} / {}", batch.total_pages)
            };
            if arabic {
                format!("الدفعة {batch_num} / {total_batches} · {page_part} · المرحلة: {phase} · {timer}")
            } else {
                format!("Batch {batch_num} / {total_batches} · {page_part} · Phase: {phase} · {timer}")
            }
        } else if arabic {
            format!("المرحلة: {phase} · {timer}")
        } else {
            format!("Phase: {phase} · {timer}")
        }
    }

    pub(crate) fn active_diagnostic_alert(&self) -> Option<DiagnosticAlert> {
        let is_benign_confirmation = self.snapshot.stage != "failed"
            && (self.snapshot.message.starts_with("AI settings saved;")
                || self.snapshot.message.starts_with("Session credential updated")
                || self.snapshot.message.starts_with("API key saved ")
                || self.snapshot.message.starts_with("Stored and session API key removed")
                || self.snapshot.message.starts_with("Protected credential presence refreshed")
                || self.snapshot.message.starts_with("Loaded ")
                || self.snapshot.message.starts_with("Owned local Ollama server started")
                || self.snapshot.message.starts_with("Stopped only the Ollama server"));
        let raw_error = if !self.error.is_empty() {
            self.error.trim()
        } else if (self.snapshot.stage == "failed" || self.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "failed"))
            && !is_benign_confirmation
            && !self.snapshot.message.trim().is_empty()
        {
            self.snapshot.message.trim()
        } else if (self.snapshot.stage == "failed"
            || (self.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "failed") && self.snapshot.stage != "idle"))
            && !is_benign_confirmation
            && let Some(last_failed) = self.snapshot.reports.last().filter(|r| r.status == "failed" && !r.message.trim().is_empty())
        {
            last_failed.message.trim()
        } else {
            return None;
        };
        if raw_error.is_empty() || is_skill_scoped_error(raw_error) {
            return None;
        }
        Some(classify_diagnostic_alert(raw_error))
    }

    pub(crate) fn clear_failure_alert(&mut self) {
        self.error.clear();
        if self.snapshot.stage == "failed" || self.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "failed") {
            self.snapshot.stage = "idle".into();
            self.snapshot.message.clear();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiagnosticAlertKind {
    ContextLimit,
    MissingCredential,
    RequestBudget,
    TokenBudget,
    CostBudget,
    InvalidModel,
    PageCountMismatch,
    LayoutPreflight,
    ConnectionOrRateLimit,
    General,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiagnosticAlert {
    pub raw_error: String,
    pub kind: DiagnosticAlertKind,
    pub title_ar: &'static str,
    pub title_en: &'static str,
    pub cause_ar: &'static str,
    pub cause_en: &'static str,
    pub solution_ar: &'static str,
    pub solution_en: &'static str,
}

pub(crate) fn classify_diagnostic_alert(raw_error: &str) -> DiagnosticAlert {
    let lower = raw_error.to_ascii_lowercase();
    let has = |terms: &[&str]| terms.iter().any(|term| lower.contains(term) || raw_error.contains(term));
    let (kind, title_ar, title_en, cause_ar, cause_en, solution_ar, solution_en) = if has(&[
        "exceeds its byte limit",
        "context limit",
        "64-kib",
        "bounded context limit",
        "size overflow",
    ]) {
        (
            DiagnosticAlertKind::ContextLimit,
            "تنبيه سعة السياق — تجاوز حجم سياق المهارات أو المراجع",
            "AI Context Capacity Alert — Skill or Reference Context Limit Exceeded",
            "تجاوز حجم إرشادات المهارات أو المراجع المدمجة الحد الأقصى المسموح به للطلب الواحد.",
            "The combined size of active skill instructions or references exceeded the per-request byte limit.",
            "اضغط على (تفعيل الدمج الذكي للمهارات) لضغط المراجع المشتركة تلقائياً أو قلل المهارات المحددة يدوياً ثم أعد المحاولة.",
            "Click 'Enable Smart Skill Compaction' to deduplicate shared references automatically or reduce manually pinned skills, then retry.",
        )
    } else if has(&["credential", "api key", "api_key", "no key", "unauthorized", "unauthenticated", "401", "403", "invalid key"]) {
        (
            DiagnosticAlertKind::MissingCredential,
            "تنبيه المصادقة — مفتاح المزود (API Key) مفقود أو غير صالح",
            "Authentication Alert — Provider API Key Missing or Unauthorized",
            "لم يتم العثور على مفتاح API صالح للمزود المحدد أو أن المفتاح الحالي منتهي الصلاحية.",
            "No valid API key was found for the selected provider, or the configured key was rejected.",
            "اضغط على (فتح المفاتيح والمزودين) لإدخال مفتاح API صالح وحفظه، أو اختر مزوداً محلياً (Ollama).",
            "Click 'Open Keys & Providers' to enter and save a valid API key, or switch to a local provider (Ollama).",
        )
    } else if has(&["request limit", "max_requests", "request budget", "request allowance", "requests exhausted", "request ceiling"]) {
        (
            DiagnosticAlertKind::RequestBudget,
            "تنبيه ميزانية الطلبات — وصلت المهمة إلى سقف عدد الطلبات",
            "Request Budget Alert — Task Reached Request Allowance Ceiling",
            "عدد الطلبات المسموح به في ميزانية المهمة أقل من العدد المطلوب لإكمال صفحات المراجعة أو الكتاب.",
            "The configured request allowance in the task budget is lower than what is needed to complete the review group or book.",
            "اضغط على (رفع حصة الطلبات تلقائياً) لضبط السقف بما يناسب صفحات الكتاب ثم اضغط إنتاج أو إعادة المحاولة.",
            "Click 'Raise Request Allowance' to match your review group or book pages, then click Produce or Retry.",
        )
    } else if has(&["token limit", "token_limit", "token budget", "max_output_tokens", "input budget", "tokens exceeded"]) {
        (
            DiagnosticAlertKind::TokenBudget,
            "تنبيه ميزانية الرموز (Tokens) — تجاوز سقف الرموز المسموح",
            "Token Budget Alert — Token Reservation Exceeded Configured Limit",
            "حجم الإدخال أو الإخراج المتوقع للمهمة يتجاوز سقف الرموز (Tokens) المحدد في الإعدادات المتقدمة.",
            "The estimated input or output tokens for this batch exceed the configured token limit in Advanced settings.",
            "اضغط على (رفع سقف الرموز تلقائياً) لزيادة ميزانية الرموز أو قلل عدد الصفحات لكل مراجعة.",
            "Click 'Raise Token Limit' to increase the token budget, or reduce Pages per review.",
        )
    } else if has(&["cost", "max_cost_usd", "price", "rates"]) {
        (
            DiagnosticAlertKind::CostBudget,
            "تنبيه سقف التكلفة — يلزم مراجعة سقف الكلفة أو أسعار المزود",
            "Cost Ceiling Alert — Cost Limit Reached or Provider Rates Unconfirmed",
            "وصلت التكلفة التقديرية إلى السقف المحدد أو لم يتم تأكيد أسعار المزود في الإعدادات المتقدمة.",
            "Estimated task cost reached the configured ceiling, or provider token rates have not been confirmed.",
            "راجع سقف التكلفة وأسعار المزود في قسم الإعدادات المتقدمة ثم أعد المحاولة.",
            "Review the cost ceiling and confirm provider rates in Advanced settings, then retry.",
        )
    } else if has(&[
        "http 404",
        "status 404",
        "404 not found",
        "model, schema, or endpoint not supported",
        "model or endpoint not found",
        "model not found",
        "was not found",
        "not found for api version",
        "retired",
        "no longer available",
        "is not supported for generatecontent",
        "does not exist",
    ]) {
        (
            DiagnosticAlertKind::InvalidModel,
            "تنبيه توفر النموذج — النموذج المحدد متقاعد أو غير موجود لدى المزود (404)",
            "Model Availability Alert — Selected Model Is Retired or Not Found (404)",
            "أعاد المزود خطأ عدم توفر النموذج (مثل gemini-2.5-flash المتقاعد أو اسم نموذج غير مدعوم على هذا المسار).",
            "The provider reported that the configured model was not found, retired (such as gemini-2.5-flash), or unsupported on this endpoint.",
            "اضغط (استخدام gemini-3.8-flash) للنصوص أو (استخدام gemini-3.1-flash-image) للصور، أو اضغط (اكتشاف النماذج المتاحة) لاختيار نموذج مدعوم.",
            "Click 'Use gemini-3.8-flash' for text or 'Use gemini-3.1-flash-image' for images, or click 'Discover Available Models' to pick from your provider.",
        )
    } else if has(&[
        "text/non-image",
        "does not generate raster images",
        "ollama image generation",
        "generated-image inspection requires",
        "inspecting generated images",
        "choose an image model",
    ]) {
        (
            DiagnosticAlertKind::InvalidModel,
            "تنبيه توافق نموذج الصور — إعداد الصور أو فحصها غير متوافق مع النموذج الحالي",
            "Image Model Compatibility Alert — Image Slot or Vision Check Mismatch",
            "تم وضع نموذج نصي أو غير مدعوم في خانة نموذج الصور، أو تفعيل فحص الصور مع نموذج نص غير متوافق.",
            "A text-only or unsupported model is configured in the image slot, or image inspection is enabled with an incompatible text model.",
            "اضغط (استخدام gemini-3.1-flash-image) أو اختر نموذج صور مدعوماً (مثل gpt-image-1)، أو أوقف فحص الصور، أو حوّل الوضع إلى عناصر قابلة للتحرير.",
            "Click 'Use gemini-3.1-flash-image' or select a supported image model (such as gpt-image-1), turn off image inspection, or switch to Editable elements.",
        )
    } else if has(&["image/media model", "non-text", "choose a text model", "missing model"]) {
        (
            DiagnosticAlertKind::InvalidModel,
            "تنبيه اختيار النموذج — النموذج المحدد لا يدعم توليد النصوص",
            "Model Selection Alert — Selected Model Does Not Support Text Planning",
            "النموذج المختار في خانة النص مخصص للصور أو الوسائط أو أن حقل النموذج فارغ.",
            "The model in the text slot is an image/media model or no text model name was entered.",
            "اختر نموذج نص قياسي (مثل gemini-3.8-flash أو gpt-4.1-mini) في حقل النموذج أعلى اللوحة.",
            "Select a standard text generation model (such as gemini-3.8-flash or gpt-4.1-mini) in the Model field at the top.",
        )
    } else if has(&["page count", "pages require reviewed batches", "explicit page", "script defines"]) {
        (
            DiagnosticAlertKind::PageCountMismatch,
            "تنبيه إعداد الصفحات — تعارض في عدد الصفحات أو نظام الدفعات",
            "Page Configuration Alert — Page Count or Batch Review Mismatch",
            "يوجد اختلاف بين عدد الصفحات المطلوب في التعليمات وعدد صفحات المستند أو إعداد المراجعة.",
            "There is a mismatch between the prompt's page count and the target document or batch review setting.",
            "طابق عدد صفحات المستند مع التعليمات وتأكد من ضبط (الصفحات لكل مراجعة) على ١ أو أكثر.",
            "Match the document page count to the prompt and ensure Pages per review is set to at least 1.",
        )
    } else if has(&["overflow", "exceeds frame", "saferect", "safe area", "preflight", "repeated body layout"]) {
        (
            DiagnosticAlertKind::LayoutPreflight,
            "تنبيه الفحص المطبعي والتخطيط — عنصر خارج الهوامش أو نص فائض",
            "Print Preflight & Layout Alert — Text Overflow or Safe-Area Violation",
            "رفض الفحص الهندسي المسبق الخطة المولدة بسبب نص فائض أو إطار يتجاوز هوامش الأمان.",
            "Native print preflight rejected the generated layout due to text frame overflow or safe-area margin bounds.",
            "أعد المحاولة ليقوم المحرك بإعادة توزيع الإطارات تلقائياً داخل منطقة الأمان، أو اختر نمط تصميم معتمد.",
            "Retry the group so the engine reflows frames within the safe area, or select an approved design style.",
        )
    } else if has(&[
        "connect",
        "timeout",
        "timed out",
        "11434",
        "8188",
        "rate limit",
        "rate or quota limit",
        "temporarily unavailable",
        "429",
        "502",
        "503",
        "504",
    ]) {
        (
            DiagnosticAlertKind::ConnectionOrRateLimit,
            "تنبيه الاتصال بالمزود — تعذّر الوصول للخادم أو تجاوز الحد الزمني",
            "Provider Connection Alert — Endpoint Unreachable or Rate Limited",
            "تعذّر الاتصال بخادم الذكاء الاصطناعي أو تم إيقاف الطلب مؤقتاً بسبب ضغط الطلبات (Rate Limit).",
            "Could not connect to the AI provider endpoint, or the provider temporarily rate-limited the request.",
            "تحقق من اتصال الإنترنت أو تشغيل الخادم المحلي (Ollama / ComfyUI) ثم أعد المحاولة بعد لحظات.",
            "Check your network connection or local server status (Ollama / ComfyUI), then retry.",
        )
    } else {
        (
            DiagnosticAlertKind::General,
            "تنبيه الذكاء الاصطناعي — تعذّر إكمال المهمة",
            "AI Production Alert — Task Could Not Complete",
            "توقفت عملية الإنتاج بسبب قيد فني أو تعذّر التحقق من مخرجات الطلب الحالي.",
            "Production stopped due to a technical validation constraint or provider execution error.",
            "راجع التفاصيل الفنية أدناه واضبط الإعدادات المقترحة ثم أعد المحاولة.",
            "Review the technical detail below, apply the suggested adjustment, and retry.",
        )
    };
    DiagnosticAlert { raw_error: raw_error.to_owned(), kind, title_ar, title_en, cause_ar, cause_en, solution_ar, solution_en }
}
