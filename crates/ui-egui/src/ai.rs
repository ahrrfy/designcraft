//! Session-only native production panel. UI actions use `dispatch` (the typed equivalent of
//! `ai.request`); the injected host must enqueue work and return immediately.
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::{DesignApp, rtl};
use designcraft_ai_types::protocol::{
    AiRequest, AiSettings, AiSnapshot, BatchSnapshot, ProductionDraft, SkillCard, TaskRequest, TaskTarget, TokenRates,
};
use designcraft_ai_types::{ArabicTashkeelMode, ModelSelection, OutputMode, ProviderKind, is_non_image_model, supports_gemini_image_review};
use serde_json::Value;

const MAX_REQUEST_ALLOWANCE: u32 = 10_000;
const MAX_TOKEN_LIMIT: u64 = 16_000_000;

pub(crate) fn is_non_text_model(model: &str) -> bool {
    designcraft_ai_types::is_non_text_model(model)
}

/// Never persisted, and never included in a debug dump.
#[derive(Clone, Default)]
struct Secret(String);

impl Drop for Secret {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

fn collect_page_ranges(pages: usize, mut predicate: impl FnMut(usize) -> bool) -> Vec<[usize; 2]> {
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

fn format_page_ranges(ranges: &[[usize; 2]], arabic: bool) -> String {
    if ranges.is_empty() {
        return text(arabic, "none", "لا شيء").to_string();
    }
    ranges.iter().map(|[start, end]| if start == end { start.to_string() } else { format!("{start}–{end}") }).collect::<Vec<_>>().join(", ")
}

struct OpenDocument {
    index: usize,
    uid: u64,
    path: Option<String>,
    title: String,
    facing_pages: bool,
    right_to_left_binding: bool,
    pages: usize,
    remaining_pages: usize,
    continuable: bool,
    filled_ranges: Vec<[usize; 2]>,
    blank_ranges: Vec<[usize; 2]>,
}
impl OpenDocument {
    fn from_document(index: usize, document: &designcraft_engine::DocState) -> Self {
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

    fn effective_filled_ranges(&self) -> Vec<[usize; 2]> {
        if !self.filled_ranges.is_empty() || !self.blank_ranges.is_empty() {
            return self.filled_ranges.clone();
        }
        let completed = self.pages.saturating_sub(self.remaining_pages);
        if completed > 0 { vec![[1, completed]] } else { Vec::new() }
    }

    fn effective_blank_ranges(&self) -> Vec<[usize; 2]> {
        if !self.filled_ranges.is_empty() || !self.blank_ranges.is_empty() {
            return self.blank_ranges.clone();
        }
        let completed = self.pages.saturating_sub(self.remaining_pages);
        if self.remaining_pages > 0 { vec![[completed.saturating_add(1), self.pages]] } else { Vec::new() }
    }

    fn page_map_line(&self, arabic: bool) -> String {
        let filled = format_page_ranges(&self.effective_filled_ranges(), arabic);
        let blank = format_page_ranges(&self.effective_blank_ranges(), arabic);
        if arabic {
            format!("خريطة الصفحات — المنجزة: {filled} · الفارغة: {blank}")
        } else {
            format!("Page map — Completed: {filled} · Blank: {blank}")
        }
    }

    fn non_continuable_reason(&self, arabic: bool) -> Option<String> {
        if self.continuable {
            return None;
        }
        let filled = self.effective_filled_ranges();
        let blank = self.effective_blank_ranges();
        if filled.is_empty() {
            Some(
                text(
                    arabic,
                    "All pages are blank; use New document or Edit current document to start from page 1.",
                    "جميع الصفحات فارغة؛ اختر مستنداً جديداً أو تحرير المستند الحالي للبدء من الصفحة 1.",
                )
                .to_string(),
            )
        } else if blank.is_empty() {
            Some(
                text(
                    arabic,
                    "All pages already contain items; there are no trailing blank pages to continue.",
                    "جميع الصفحات تحتوي على عناصر؛ لا توجد صفحات فارغة متبقية للاستكمال.",
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

struct RequestEstimate {
    review_pages: u32,
    remaining_pages: u32,
    remaining_groups: u32,
    book_allowance: u32,
    scripted: bool,
    image_allowance: u32,
    vision_allowance: u32,
}

#[derive(Clone, Copy)]
struct TokenEstimate {
    per_page_tokens: u64,
    review_tokens: u64,
    book_tokens: u64,
}

#[derive(Clone)]
struct CompanionProof {
    path: String,
    texture: Option<egui::TextureHandle>,
    error: String,
}

/// Global zero-based pages: the cover stands alone, followed by 2–3, 4–5, … .
fn facing_proof_pair(page: u32, right_to_left: bool) -> Option<[u32; 2]> {
    if page == 0 {
        return None;
    }
    let first = if page.is_multiple_of(2) { page.checked_sub(1)? } else { page };
    let second = first.checked_add(1)?;
    Some(if right_to_left { [second, first] } else { [first, second] })
}

fn automatic_proof_binding(brief: &str, fallback: bool) -> bool {
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
    fn allowance(&self) -> u32 {
        self.review_pages.saturating_add(self.image_allowance).saturating_add(self.vision_allowance)
    }
}

/// A display estimate only: runtime still validates all authored PAGE blocks and fields.
fn explicit_script_pages(brief: &str) -> Option<u32> {
    if brief.len() > designcraft_ai_types::protocol::MAX_BRIEF_BYTES || brief.contains('\0') {
        return None;
    }
    let mut pages = 0u32;
    let mut fenced = false;
    for line in brief.lines() {
        let line = line.trim();
        if line.starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced || !line.starts_with('#') {
            continue;
        }
        let heading = line.trim_start_matches('#').trim_start();
        if heading.get(..4).is_none_or(|prefix| !prefix.eq_ignore_ascii_case("page")) {
            continue;
        }
        let rest = heading.get(4..)?;
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        let rest = rest.trim_start();
        let boundary = rest.find(|character: char| !character.is_ascii_digit()).unwrap_or(rest.len());
        let number = rest.get(..boundary)?.parse::<u32>().ok()?;
        if number != pages.checked_add(1)? || number > 1000 {
            return None;
        }
        let suffix = rest.get(boundary..)?.trim_start();
        let title = suffix
            .strip_prefix('—')
            .or_else(|| suffix.strip_prefix('–'))
            .or_else(|| suffix.strip_prefix('-'))
            .or_else(|| suffix.strip_prefix(':'))?
            .trim();
        if title.is_empty() || title.chars().count() > 512 {
            return None;
        }
        pages = number;
    }
    (pages > 0).then_some(pages)
}

pub(crate) fn is_retired_gemini_text_model(provider: ProviderKind, model: &str) -> bool {
    designcraft_ai_types::is_retired_gemini_model(provider, model)
}

/// Search-only normalization matching runtime style keyword matching; prompt text stays exact.
fn normalize_skill_search_ui(text: &str) -> String {
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
fn natural_prompt_pages(prompt: &str) -> Option<u32> {
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
    inline_candidate
}
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

/// Human review belongs to one complete, immutable proof manifest and document revision.
#[derive(Clone, PartialEq, Eq)]
struct BatchReviewKey {
    job_id: String,
    checkpoint: u32,
    batch: u32,
    document: (u64, u64),
    page_range: (u32, u32),
    task_shape: (u32, u32, u32),
    proofs: Vec<(String, String)>,
}
impl BatchReviewKey {
    fn from_batch(batch: &BatchSnapshot) -> Option<Self> {
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

    fn pages(&self) -> impl Iterator<Item = &(String, String)> {
        self.proofs.iter().filter(|(path, _)| path.to_ascii_lowercase().ends_with(".png"))
    }

    fn same_proof_except_revision(&self, other: &Self) -> bool {
        let mut normalized = other.clone();
        normalized.document.1 = self.document.1;
        self == &normalized
    }
}

/// A saved proof is display data, never authority to approve or resume a worker.
#[derive(Clone, PartialEq, Eq)]
struct ProofDisplay {
    job_id: String,
    batch: u32,
    page_range: (u32, u32),
    proofs: Vec<(String, String)>,
}
impl ProofDisplay {
    fn from_batch(batch: &BatchSnapshot) -> Option<Self> {
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

    fn pages(&self) -> impl Iterator<Item = &(String, String)> {
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
    initialized: bool,
    last_poll: Option<Instant>,
    api_key: Secret,
    key_for_images: bool,
    prompt: String,
    new_document: bool,
    continue_book: bool,
    restored_target_unconfirmed: bool,
    pages: u32,
    automatic_skills: bool,
    selected_skills: BTreeSet<String>,
    directory: String,
    import_path: String,
    export_path: String,
    skill_export_path: String,
    skill_prompt: String,
    build_design_style: bool,
    correction_before: String,
    correction_after: String,
    correction_reason: String,
    references: String,
    review_key: Option<(String, String)>,
    skill_md: String,
    manifest_json: String,
    preview_path: Option<String>,
    preview: Option<egui::TextureHandle>,
    preview_error: String,
    error: String,
    printer: String,
    grayscale: bool,
    duplex: String,
    rates_confirmed: bool,
    workflow_json: String,
    workflow_error: String,
    proof_key: Option<BatchReviewKey>,
    proof_display: Option<ProofDisplay>,
    show_saved_proof: bool,
    reviewed_proofs: BTreeSet<(String, String)>,
    proof_document_version: Option<(u64, u64)>,
    proof_validation_requested: Option<(u64, u64)>,
    proof_document_matches: bool,
    proof_facing_pages: Option<bool>,
    proof_binding: Option<bool>,
    proof_pair_view: bool,
    proof_full_resolution: bool,
    review_input_is_human: bool,
    review_pointer_press: Option<(BatchReviewKey, egui::Id)>,
    batch_approval_gesture: Option<(BatchReviewKey, egui::Context, u64)>,
    proof_path: Option<String>,
    proof_loaded: Option<String>,
    proof_texture: Option<egui::TextureHandle>,
    companion_proof: Option<CompanionProof>,
    proof_error: String,
    local_ollama: ModelSelection,
    local_comfy: ModelSelection,
    pending_models: Option<ModelSelection>,
    local_probe: Option<(ModelSelection, Vec<String>, String, bool)>,
    discovered_models: BTreeMap<(ProviderKind, Option<String>), Vec<String>>,
    print_token: Option<String>,
    print_reviewed: bool,
    print_save_path: String,
    ollama_executable: String,
    pick_ollama: bool,
    open_document_path: String,
    pick_target_document: bool,
    open_target_document_requested: bool,
    expand_advanced_settings: Option<bool>,
    polish_book_requested: bool,
    polish_book_status: String,
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
impl Default for AiUiState {
    fn default() -> Self {
        Self {
            open: false,
            preview_update: crate::preview_update::PreviewUpdate::default(),
            tab: 0,
            snapshot: AiSnapshot::default(),
            settings: AiSettings::default(),
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
            expand_advanced_settings: None,
            polish_book_requested: false,
            polish_book_status: String::new(),
        }
    }
}

fn rate_table_configured(rates: &TokenRates) -> bool {
    rates.input_per_million > 0.0
        || rates.output_per_million > 0.0
        || rates.image_per_request.is_some_and(|v| v > 0.0)
        || rates.image_reserve_usd.is_some_and(|v| v > 0.0)
}

fn has_configured_rates(settings: &AiSettings) -> bool {
    let text_ok = settings.budget.rates.as_ref().is_some_and(|rates| rates.input_per_million > 0.0 || rates.output_per_million > 0.0);
    if settings.output_mode == OutputMode::NativeWithImages {
        let image_rates = settings.image_rates.as_ref().or(settings.budget.rates.as_ref());
        text_ok && image_rates.is_some_and(rate_table_configured)
    } else {
        text_ok
    }
}

impl AiUiState {
    fn load_production_request(&mut self, request: &TaskRequest) {
        self.prompt = request.prompt.clone();
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

    fn sync_settings(&mut self, settings: AiSettings) {
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

    fn accept(&mut self, snapshot: AiSnapshot) {
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

    fn sync_proof_document(&mut self, document: Option<(u64, u64)>) {
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

    fn effective_proof_facing_pages(&self) -> bool {
        self.proof_facing_pages.unwrap_or(false) || self.settings.facing_pages
    }

    fn can_review_selected_proof(&self) -> bool {
        self.review_input_is_human
            && !self.snapshot.busy
            && self.proof_document_matches
            && self.proof_texture.is_some()
            && self.proof_error.is_empty()
            && self.proof_loaded == self.proof_path
            && self.proof_path.as_ref().is_some_and(|path| self.proof_key.as_ref().is_some_and(|key| key.pages().any(|(page, _)| page == path)))
    }

    /// Only an explicit per-page UI gesture records review; preview loading never does.
    fn review_selected_proof(&mut self) {
        if !self.can_review_selected_proof() {
            return;
        }
        if let Some(page) = self.proof_key.as_ref().and_then(|key| key.pages().find(|(path, _)| self.proof_path.as_deref() == Some(path.as_str()))) {
            self.reviewed_proofs.insert(page.clone());
        }
    }

    /// Navigation never carries a loaded texture or a held review gesture to another page.
    fn select_proof(&mut self, path: &str) -> bool {
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

    fn companion_proof_path(&self) -> Option<&str> {
        if !self.effective_proof_facing_pages() || !self.proof_pair_view {
            return None;
        }
        let key = self.proof_display.as_ref()?;
        let selected = key.pages().position(|(path, _)| self.proof_path.as_deref() == Some(path.as_str()))?;
        let page = key.page_range.0.checked_add(u32::try_from(selected).ok()?)?;
        let rtl = self.proof_binding.or(self.settings.right_to_left_binding).unwrap_or_else(|| automatic_proof_binding(&self.prompt, false));
        let pair = facing_proof_pair(page, rtl)?;
        let companion = pair.into_iter().find(|candidate| *candidate != page)?;
        let index = companion.checked_sub(key.page_range.0)?;
        key.pages().nth(usize::try_from(index).ok()?).map(|(path, _)| path.as_str())
    }

    fn can_review_pair(&self) -> bool {
        self.proof_full_resolution
            && self.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "awaiting_human_proof")
            && self.can_review_selected_proof()
            && self.companion_proof_path().is_some_and(|path| {
                self.companion_proof.as_ref().is_some_and(|proof| proof.path == path && proof.texture.is_some() && proof.error.is_empty())
            })
    }

    fn production_task(&self) -> TaskRequest {
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

    fn review_displayed_pair(&mut self) {
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

    fn can_approve_batch(&self) -> bool {
        self.review_input_is_human
            && !self.snapshot.busy
            && self.proof_document_matches
            && self.proof_error.is_empty()
            && self.proof_key.as_ref().is_some_and(|key| key.pages().all(|page| self.reviewed_proofs.contains(page)))
    }

    /// A clicked response alone may come from accessibility or stale widget state.
    /// Bind a primary press/release (or non-repeated focused key press) to this proof.
    fn fresh_review_gesture(
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

    fn task_active(&self) -> bool {
        self.snapshot.busy || self.snapshot.batch.as_ref().is_some_and(|b| matches!(b.status.as_str(), "running" | "awaiting_human_proof"))
    }

    /// Parse only after an edit. The raw draft survives partial JSON, repaints and host polls.
    fn edit_workflow(&mut self) {
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

    fn workflow_ready(&self) -> bool {
        self.settings.output_mode != OutputMode::NativeWithImages
            || self.settings.image_model.provider != ProviderKind::ComfyUi
            || (self.workflow_error.is_empty() && self.settings.comfy_workflow.is_some())
    }

    fn production_issue(&self, available: bool, document: bool, arabic: bool) -> Option<&'static str> {
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
        } else if self.settings.output_mode == OutputMode::NativeWithImages && self.settings.image_model.provider == ProviderKind::Ollama {
            (
                "Ollama does not generate raster images here. Choose Gemini, OpenAI, or ComfyUI for the image model, or switch to Editable elements.",
                "مزود Ollama لا يولّد صوراً نقطية هنا. اختر Gemini أو OpenAI أو ComfyUI لنموذج الصور، أو حوّل الوضع إلى عناصر قابلة للتحرير.",
            )
        } else if self.settings.output_mode == OutputMode::NativeWithImages
            && self.settings.image_model.provider != ProviderKind::ComfyUi
            && self.settings.image_model.model.trim().is_empty()
        {
            ("Choose an image model in Providers and keys.", "اختر نموذج الصور من تبويب المزودين والمفاتيح.")
        } else if self.settings.output_mode == OutputMode::NativeWithImages
            && !self.settings.image_model.model.trim().is_empty()
            && is_non_image_model(self.settings.image_model.provider, &self.settings.image_model.model)
        {
            (
                "The selected image model is a text/non-image or unsupported endpoint model. Choose a supported image generation model (such as gemini-3.1-flash-image or gpt-image-1) or switch to Editable elements.",
                "نموذج الصور المختار مخصص للنصوص أو غير مدعوم على مسار التوليد الحالي. اختر نموذج توليد صور مدعوماً (مثل gemini-3.1-flash-image أو gpt-image-1) أو حوّل الوضع إلى عناصر قابلة للتحرير.",
            )
        } else if self.settings.output_mode == OutputMode::NativeWithImages
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

    fn effective_design_profile(&self) -> Option<&designcraft_ai_types::DesignProfile> {
        if let Some(id) = &self.settings.design_style {
            return self
                .snapshot
                .skills
                .iter()
                .rev()
                .filter(|card| &card.id == id && card.design_profile.is_some())
                .max_by_key(|card| u8::from(card.enabled && card.compatible) * 2 + u8::from(card.active))
                .and_then(|card| card.design_profile.as_ref());
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
                .find_map(|card| card.design_profile.as_ref());
        }
        if let Some(profile) = self
            .selected_skills
            .iter()
            .filter_map(|id| {
                self.snapshot
                    .skills
                    .iter()
                    .rev()
                    .find(|card| &card.id == id && card.enabled && card.compatible && card.active && card.design_profile.is_some())
            })
            .find_map(|card| card.design_profile.as_ref())
        {
            return Some(profile);
        }
        let lower = normalize_skill_search_ui(&self.prompt);
        if lower.trim().is_empty() {
            return None;
        }
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
                let bonus = usize::from(hits > 0 && card.id == "kg-oxford-cambridge-style") * 16;
                (hits > 0).then_some((hits.saturating_add(bonus), card.id.as_str(), profile))
            })
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))
            .map(|(_, _, profile)| profile)
    }

    fn request_estimate(&self, documents: &[OpenDocument], active: Option<usize>) -> Option<RequestEstimate> {
        if (!self.new_document && !self.continue_book) || self.settings.pages_per_batch == 0 || self.prompt.trim().is_empty() {
            return None;
        }
        let scripted_pages = explicit_script_pages(&self.prompt);
        let remaining = if self.new_document {
            if scripted_pages.is_some_and(|pages| pages != self.pages) {
                return None;
            }
            self.pages
        } else {
            let document = documents.iter().find(|document| Some(document.index) == active)?;
            if scripted_pages.is_some_and(|pages| u32::try_from(document.pages).ok() != Some(pages)) || (self.continue_book && !document.continuable)
            {
                return None;
            }
            u32::try_from(if self.continue_book { document.remaining_pages } else { document.pages }).ok()?
        };
        let spread_aligned_new = self.new_document && self.settings.facing_pages && self.settings.pages_per_batch == 2 && remaining > 1;
        let review_pages = if spread_aligned_new { 1 } else { remaining.min(self.settings.pages_per_batch) };
        let sources_per_page = if self.settings.output_mode == OutputMode::NativeOnly {
            0
        } else {
            self.effective_design_profile()
                .map(|profile| profile.page_families.iter().map(|family| family.image_slots.len()).max().unwrap_or(0))
                .map_or(1, |sources| u32::try_from(sources.clamp(1, 8)).unwrap_or(8))
        };
        let image_allowance = review_pages.saturating_mul(sources_per_page);
        let vision_allowance = if self.settings.verify_generated_images { image_allowance } else { 0 };
        let per_page = 1u32.saturating_add(sources_per_page).saturating_add(if self.settings.verify_generated_images { sources_per_page } else { 0 });
        (review_pages > 0).then_some(RequestEstimate {
            review_pages,
            remaining_pages: remaining,
            remaining_groups: if spread_aligned_new {
                1u32.saturating_add(remaining.saturating_sub(1).div_ceil(2))
            } else {
                remaining.div_ceil(self.settings.pages_per_batch)
            },
            book_allowance: remaining.saturating_mul(per_page),
            scripted: scripted_pages.is_some(),
            image_allowance,
            vision_allowance,
        })
    }

    fn token_estimate(&self, documents: &[OpenDocument], active: Option<usize>) -> Option<TokenEstimate> {
        let estimate = self.request_estimate(documents, active)?;
        let profile = self.effective_design_profile();
        let overhead_bytes: u64 = if profile.is_some() {
            96_000
        } else if self.automatic_skills || !self.selected_skills.is_empty() {
            176 * 1024
        } else {
            48_000
        };
        let prompt_bytes = u64::try_from(self.prompt.len()).unwrap_or(u64::MAX);
        let text_per_page = prompt_bytes
            .saturating_add(overhead_bytes)
            .div_ceil(3)
            .saturating_add(1024)
            .saturating_add(u64::from(self.settings.budget.max_output_tokens));
        let sources_per_page = estimate.image_allowance.checked_div(estimate.review_pages).map_or(0, u64::from);
        let vision_per_page = if self.settings.output_mode == OutputMode::NativeWithImages {
            let workflow_tokens = self.settings.comfy_workflow.as_ref().map_or(0, |v| u64::try_from(v.to_string().len()).unwrap_or(0).div_ceil(3));
            let image_call_tokens = 2_048u64.saturating_add(1_024).saturating_add(workflow_tokens);
            let review_call_tokens = if self.settings.verify_generated_images {
                8_192u64.saturating_add(1_024).saturating_add(4_096).saturating_add(u64::from(self.settings.budget.max_output_tokens.min(4096)))
            } else {
                0
            };
            sources_per_page.saturating_mul(image_call_tokens.saturating_add(review_call_tokens))
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

    fn live_phase(stage: &str, arabic: bool) -> &'static str {
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

    fn extract_message_page(message: &str, total_pages: u32) -> Option<(u32, u32)> {
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

    fn live_status_line(&self, arabic: bool) -> String {
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
        if raw_error.is_empty() {
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

/// Typed UI path shared by all panel buttons. Credentials move directly to the host without JSON,
/// logs, serialization, document metadata or preference storage.
pub fn dispatch(app: &mut DesignApp, request: AiRequest) -> Result<Value, String> {
    if !human_review_input(app)
        && matches!(
            &request,
            AiRequest::ApproveSkill { .. }
                | AiRequest::ApproveBatch { .. }
                | AiRequest::SetApiKey { .. }
                | AiRequest::SaveApiKey { .. }
                | AiRequest::RemoveApiKey { .. }
                | AiRequest::RefreshCredentials
                | AiRequest::PrintDocument { .. }
                | AiRequest::SubmitPreparedPrint { .. }
                | AiRequest::OpenPrintProof { .. }
                | AiRequest::StartLocalOllama { .. }
                | AiRequest::StopLocalOllama
        )
    {
        return Err("This action requires a human gesture in the desktop panel; control-generated input is not approval.".into());
    }
    if let AiRequest::ApproveBatch { job_id, checkpoint } = &request {
        let gesture = app.ui.ai.batch_approval_gesture.take();
        app.ui.ai.review_input_is_human = human_review_input(app);
        app.ui.ai.sync_proof_document(app.session.active().map(|document| (document.uid, document.revision)));
        if !gesture.is_some_and(|(key, ctx, frame)| app.ui.ai.proof_key.as_ref() == Some(&key) && ctx.cumulative_frame_nr() == frame)
            || !app.ui.ai.can_approve_batch()
            || !app.ui.ai.proof_key.as_ref().is_some_and(|key| key.job_id == *job_id && key.checkpoint == *checkpoint)
        {
            return Err("Review and confirm every page of the current document proof before approving this checkpoint.".into());
        }
        app.ui.ai.reviewed_proofs.clear();
    }
    let Some(host) = app.services.ai.as_mut() else {
        let e = "Native AI host is unavailable in this build.".to_string();
        app.ui.ai.error = e.clone();
        return Err(e);
    };
    if matches!(&request, AiRequest::InspectSkill { .. }) {
        app.ui.ai.review_key = None;
    }
    if matches!(&request, AiRequest::RunSkillSample { .. } | AiRequest::InspectSkill { .. }) {
        app.ui.ai.preview_path = None;
        app.ui.ai.preview = None;
    }
    app.ui.ai.error.clear();
    if let AiRequest::ListModels { selection } = &request
        && !app.ui.ai.task_active()
    {
        app.ui.ai.pending_models = Some(selection.clone());
    }
    let updated_settings = matches!(&request, AiRequest::UpdateSettings(_));
    let refreshed_proofs = matches!(&request, AiRequest::RefreshBatchProofs { .. });
    let production_request = if let AiRequest::Start(request) = &request { Some(request.clone()) } else { None };
    let snapshot = host(&mut app.session, request);
    let result = serde_json::to_value(&snapshot).map_err(|e| e.to_string());
    app.ui.ai.accept(snapshot);
    if refreshed_proofs {
        app.ui.ai.sync_proof_document(app.session.active().map(|document| (document.uid, document.revision)));
    }
    if updated_settings && app.ui.ai.snapshot.stage != "failed" {
        let next_settings = app.ui.ai.snapshot.settings.clone();
        app.ui.ai.sync_settings(next_settings);
    }
    if let Some(alert) = app.ui.ai.active_diagnostic_alert() {
        app.ui.status = format!("⚠ {} | {}", alert.title_ar, alert.title_en);
    }
    if let Some(request) = production_request {
        app.ui.ai.load_production_request(&request);
    }
    result
}

fn human_review_input(app: &DesignApp) -> bool {
    !app.in_control && !app.control_input_frame.load(std::sync::atomic::Ordering::Relaxed)
}

/// Control channel / MCP command path: `ai.request {method, ...}`. Methods match the snake_case
/// AiRequest variants. `start` accepts `request: TaskRequest`; settings are `settings: AiSettings`;
/// model discovery accepts `selection: ModelSelection`. All other fields match the typed variant.
/// Credentials, human approvals, physical print submission and owned-process launch use only
/// typed panel controls. Secret input never enters control transcripts.
/// Control/MCP automation may tighten spending budgets, never raise them above the user's ceiling.
fn clamp_control_settings(ceiling: &AiSettings, settings: &mut AiSettings) {
    settings.budget.max_requests = settings.budget.max_requests.min(ceiling.budget.max_requests);
    settings.budget.token_limit = settings.budget.token_limit.min(ceiling.budget.token_limit);
    settings.budget.max_output_tokens = settings.budget.max_output_tokens.min(ceiling.budget.max_output_tokens);
    settings.budget.repair_attempts = settings.budget.repair_attempts.min(ceiling.budget.repair_attempts);
    if let Some(cap) = ceiling.budget.max_cost_usd {
        settings.budget.max_cost_usd = Some(settings.budget.max_cost_usd.map_or(cap, |v| v.min(cap)));
    }
    // Control cannot spoof rate tables or escalate into paid repairs, image generation, or vision review.
    settings.budget.rates = ceiling.budget.rates.clone();
    settings.image_rates = ceiling.image_rates.clone();
    if !ceiling.allow_paid_repairs {
        settings.allow_paid_repairs = false;
    }
    if ceiling.output_mode == OutputMode::NativeOnly {
        settings.output_mode = OutputMode::NativeOnly;
    }
    if !ceiling.verify_generated_images {
        settings.verify_generated_images = false;
    }
}

fn clamped_settings(ceiling: &AiSettings, raw: Value) -> Result<AiSettings, String> {
    let mut settings: AiSettings = serde_json::from_value(raw).map_err(|e| e.to_string())?;
    clamp_control_settings(ceiling, &mut settings);
    Ok(settings)
}

fn ensure_ceiling_initialized(app: &mut DesignApp) {
    if !app.ui.ai.initialized && app.services.ai.is_some() {
        let _ = dispatch(app, AiRequest::Poll);
    }
}

pub fn request_command(app: &mut DesignApp, p: &Value) -> Result<Value, String> {
    let field = |name: &str| p.get(name).and_then(Value::as_str).map(str::to_owned).ok_or_else(|| format!("missing string: {name}"));
    let decode = |name: &str| p.get(name).cloned().ok_or_else(|| format!("missing {name}"));
    let request = match field("method")?.as_str() {
        "poll" => AiRequest::Poll,
        "cancel" => AiRequest::Cancel,
        "start" => {
            ensure_ceiling_initialized(app);
            let mut req: TaskRequest = serde_json::from_value(decode("request")?).map_err(|e| e.to_string())?;
            clamp_control_settings(&app.ui.ai.settings, &mut req.settings);
            AiRequest::Start(req)
        }
        "preview_production" => AiRequest::PreviewProduction(serde_json::from_value(decode("request")?).map_err(|e| e.to_string())?),
        "update_settings" => {
            ensure_ceiling_initialized(app);
            AiRequest::UpdateSettings(clamped_settings(&app.ui.ai.settings, decode("settings")?)?)
        }
        "list_models" => AiRequest::ListModels { selection: serde_json::from_value(decode("selection")?).map_err(|e| e.to_string())? },
        "import_skill" => AiRequest::ImportSkill { path: field("path")? },
        "inspect_skill" => AiRequest::InspectSkill { id: field("id")?, revision: field("revision")? },
        "prepare_skills" => AiRequest::PrepareSkills { prompt: field("prompt")? },
        "save_skill_draft" => AiRequest::SaveSkillDraft { id: field("id")?, skill_md: field("skill_md")?, manifest_json: field("manifest_json")? },
        "add_design_correction" => AiRequest::AddDesignCorrection {
            id: field("id")?,
            revision: field("revision")?,
            correction: serde_json::from_value(decode("correction")?).map_err(|e| e.to_string())?,
        },
        "generate_skill" => {
            ensure_ceiling_initialized(app);
            AiRequest::GenerateSkill {
                design_style: p.get("design_style").map(|v| v.as_bool().ok_or("design_style must be boolean")).transpose()?.unwrap_or(false),
                prompt: field("prompt")?,
                reference_paths: serde_json::from_value(decode("reference_paths")?).map_err(|e| e.to_string())?,
                settings: clamped_settings(&app.ui.ai.settings, decode("settings")?)?,
            }
        }
        "generate_skill_from_document" => {
            ensure_ceiling_initialized(app);
            AiRequest::GenerateSkillFromDocument {
                design_style: p.get("design_style").map(|v| v.as_bool().ok_or("design_style must be boolean")).transpose()?.unwrap_or(false),
                prompt: field("prompt")?,
                settings: clamped_settings(&app.ui.ai.settings, decode("settings")?)?,
            }
        }
        "run_skill_sample" => AiRequest::RunSkillSample { id: field("id")?, revision: field("revision")? },
        "run_design_template" => AiRequest::RunDesignTemplate { id: field("id")?, revision: field("revision")?, template: field("template")? },
        "refresh_batch_proofs" => AiRequest::RefreshBatchProofs { job_id: field("job_id")? },
        "approve_skill" | "approve_batch" => {
            return Err("Approval requires the human review button in the desktop panel.".into());
        }
        "save_api_key" | "remove_api_key" | "refresh_credentials" => {
            return Err("Credential actions require the masked human controls in Providers and keys.".into());
        }
        "refresh_resources" => AiRequest::RefreshResources,
        "set_skill_enabled" => {
            AiRequest::SetSkillEnabled { id: field("id")?, enabled: p.get("enabled").and_then(Value::as_bool).ok_or("missing enabled")? }
        }
        "rollback_skill" => AiRequest::RollbackSkill { id: field("id")?, revision: field("revision")? },
        "export_skill" => AiRequest::ExportSkill { id: field("id")?, revision: field("revision")?, path: field("path")? },
        "create_science_sample" => AiRequest::CreateScienceSample { directory: field("directory")? },
        "export_document" => AiRequest::ExportDocument { path: field("path")?, pdf: p.get("pdf").and_then(Value::as_bool).ok_or("missing pdf")? },
        "export_idml" => AiRequest::ExportIdml { path: field("path")? },
        "export_indd" => AiRequest::ExportIndd { path: field("path")? },
        "prepare_print" => AiRequest::PreparePrint {
            printer: p.get("printer").and_then(Value::as_str).map(str::to_owned),
            grayscale: p.get("grayscale").and_then(Value::as_bool).unwrap_or(false),
            duplex: field("duplex")?,
        },
        "save_prepared_print" => AiRequest::SavePreparedPrint { token: field("token")?, path: field("path")? },
        "print_document" | "submit_prepared_print" | "open_print_proof" | "start_local_ollama" | "stop_local_ollama" => {
            return Err("Printing and local service launch require the explicit human controls in the desktop panel.".into());
        }
        _ => return Err("Unknown AI request method; human-only actions require desktop controls.".into()),
    };
    dispatch(app, request)
}

pub fn poll(app: &mut DesignApp, ctx: &egui::Context) {
    if !app.ui.ai.snapshot.busy {
        return;
    }
    let due = app.ui.ai.last_poll.is_none_or(|t| t.elapsed() >= Duration::from_millis(150));
    if due {
        app.ui.ai.last_poll = Some(Instant::now());
        let _ = dispatch(app, AiRequest::Poll);
    }
    ctx.request_repaint_after(Duration::from_millis(150));
}

fn text<'a>(arabic: bool, en: &'a str, ar: &'a str) -> &'a str {
    if arabic { ar } else { en }
}
fn label(ui: &mut egui::Ui, arabic: bool, en: &str, ar: &str) {
    rtl::label(ui, text(arabic, en, ar));
}
fn button(ui: &mut egui::Ui, arabic: bool, en: &str, ar: &str) -> bool {
    ui.button(rtl::widget(ui, text(arabic, en, ar))).clicked()
}
fn bidi_layouter(multiline: bool) -> impl FnMut(&egui::Ui, &dyn egui::TextBuffer, f32) -> std::sync::Arc<egui::Galley> {
    move |ui, buf, wrap_width| {
        let font_id = egui::TextStyle::Body.resolve(ui.style());
        let color = ui.visuals().text_color();
        rtl::editable_layout(ui, buf.as_str(), wrap_width, font_id, color, multiline)
    }
}
fn field(ui: &mut egui::Ui, arabic: bool, en: &str, ar: &str, value: &mut String) {
    label(ui, arabic, en, ar);
    let field_rtl = if value.trim().is_empty() { arabic } else { rtl::has_rtl(value) };
    let mut layouter = bidi_layouter(false);
    rtl::show_text_edit(
        ui,
        egui::TextEdit::singleline(value)
            .desired_width(f32::INFINITY)
            .horizontal_align(if field_rtl { egui::Align::Max } else { egui::Align::Min })
            .layouter(&mut layouter),
    );
}
fn provider_name(p: ProviderKind) -> &'static str {
    match p {
        ProviderKind::OpenAi => "OpenAI",
        ProviderKind::Gemini => "Gemini",
        ProviderKind::Ollama => "Ollama",
        ProviderKind::ComfyUi => "ComfyUI",
    }
}
fn provider(ui: &mut egui::Ui, id: &str, selected: &mut ProviderKind, images: bool) {
    egui::ComboBox::from_id_salt(id).selected_text(provider_name(*selected)).show_ui(ui, |ui| {
        for p in [ProviderKind::OpenAi, ProviderKind::Gemini, if images { ProviderKind::ComfyUi } else { ProviderKind::Ollama }] {
            ui.selectable_value(selected, p, provider_name(p));
        }
    });
}
fn model(
    ui: &mut egui::Ui,
    a: bool,
    id: &str,
    m: &mut ModelSelection,
    images: bool,
    pending_models: &mut Option<ModelSelection>,
    discovered: &BTreeMap<(ProviderKind, Option<String>), Vec<String>>,
    requests: &mut Vec<AiRequest>,
) {
    ui.horizontal_wrapped(|ui| {
        provider(ui, id, &mut m.provider, images);
        label(ui, a, "Model", "النموذج");
        ui.add(egui::TextEdit::singleline(&mut m.model).desired_width(240.0));
        if button(ui, a, "Discover", "اكتشاف") {
            *pending_models = Some(m.clone());
            requests.push(AiRequest::ListModels { selection: m.clone() });
        }
    });
    if let Some(models) = discovered.get(&(m.provider, m.endpoint.clone())) {
        let filtered: Vec<&String> =
            models.iter().filter(|name| if images { !is_non_image_model(m.provider, name) } else { !is_non_text_model(name) }).collect();
        if !filtered.is_empty() {
            let picker_label = if images {
                text(a, "Available image models", "نماذج الصور المتاحة")
            } else {
                text(a, "Available models", "النماذج المتاحة")
            };
            egui::ComboBox::from_id_salt((id, "discovered_models")).selected_text(rtl::widget(ui, picker_label)).show_ui(ui, |ui| {
                for name in filtered {
                    ui.selectable_value(&mut m.model, name.clone(), name);
                }
            });
        }
    }
    let mut endpoint = m.endpoint.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        label(ui, a, "Endpoint (optional)", "عنوان الخدمة (اختياري)");
        if ui.add(egui::TextEdit::singleline(&mut endpoint).desired_width(350.0)).changed() {
            m.endpoint = (!endpoint.trim().is_empty()).then_some(endpoint);
        }
    });
}

pub fn show(app: &mut DesignApp, ctx: &egui::Context) {
    if !app.ui.ai.open {
        return;
    }
    let document = app.session.active().map(|document| (document.uid, document.revision));
    let needs_validation = !app.ui.ai.snapshot.busy
        && app.ui.ai.snapshot.batch.as_ref().is_some_and(|batch| {
            batch.status == "awaiting_human_proof"
                && document.is_some_and(|(uid, revision)| Some(uid) == batch.document_uid && Some(revision) != batch.document_revision)
        });
    if needs_validation && app.ui.ai.proof_validation_requested != document && app.services.ai.is_some() {
        app.ui.ai.proof_validation_requested = document;
        // Selection and page navigation advance the cache revision without changing content.
        // Keep review evidence disabled until the host validates the pinned document allocation,
        // font cache and preferences; a rejected version is requested only once.
        app.ui.ai.sync_proof_document(document);
        if let Err(error) = dispatch(app, AiRequest::Poll) {
            app.ui.ai.error = error;
        }
    }
    let mut state = std::mem::take(&mut app.ui.ai);
    state.batch_approval_gesture = None;
    state.review_input_is_human = human_review_input(app);
    state.sync_proof_document(app.session.active().map(|document| (document.uid, document.revision)));
    state.proof_facing_pages =
        app.session.active().filter(|document| state.proof_document_matches && document.doc.settings.facing_pages).map(|_| true);
    state.proof_binding = app
        .session
        .active()
        .filter(|document| state.proof_document_matches && document.doc.settings.facing_pages)
        .map(|document| document.doc.settings.right_to_left_binding);
    let a = app.ui.language == "ar";
    let mut open = state.open;
    let mut requests = Vec::new();
    let mut check_preview_update = false;
    let mut toggle_language = false;
    let available = app.services.ai.is_some() && state.snapshot.available;
    let documents: Vec<_> =
        app.session.documents().iter().enumerate().map(|(index, document)| OpenDocument::from_document(index, document)).collect();
    let mut activate_document = app.session.active_index();
    let original_document = activate_document;
    egui::Window::new(rtl::widget_ctx(ctx, text(a, "AI Production and Skills", "الذكاء")))
        .id(egui::Id::new("ai_production"))
        .open(&mut open)
        .default_size(egui::vec2(720.0, 720.0))
        .min_width(540.0)
        .resizable(true)
        .show(ctx, |ui| {
            check_preview_update = state.preview_update.show(ui, a);
            ui.separator();
            ui.horizontal(|ui| {
                for (i, en, ar) in [
                    (0, "Production", "الإنتاج"),
                    (1, "Skills", "المهارات"),
                    (2, "Usage", "الاستخدام"),
                    (3, "Providers and keys", "المزودون والمفاتيح"),
                    (4, "Local AI", "الذكاء المحلي"),
                ] {
                    if ui.selectable_label(state.tab == i, rtl::widget(ui, text(a, en, ar))).clicked() {
                        state.tab = i;
                    }
                }
                if button(ui, a, "Refresh", "تحديث") {
                    requests.push(AiRequest::Poll);
                }
                if ui.button(rtl::widget(ui, if a { "EN" } else { "العربية" })).clicked() {
                    toggle_language = true;
                }
            });
            ui.separator();
            if !available {
                label(
                    ui,
                    a,
                    "Native AI host is unavailable. Open this panel in the desktop application.",
                    "خدمة الذكاء الاصطناعي غير متاحة. افتح هذه اللوحة في تطبيق سطح المكتب.",
                );
            }
            if state.task_active() {
                ui.add(egui::ProgressBar::new(state.snapshot.progress.clamp(0.0, 1.0)).text(&state.snapshot.stage));
                let usage = &state.snapshot.current_usage;
                rtl::label(
                    ui,
                    format!(
                        "{}: {} / {} · {} {} · {} {}{}{}",
                        text(a, "Live requests", "الطلبات المباشرة"),
                        usage.requests,
                        state.settings.budget.max_requests,
                        text(a, "Input", "الإدخال"),
                        usage.input_tokens,
                        text(a, "Output", "الإخراج"),
                        usage.output_tokens,
                        usage.estimated_cost_usd.map_or_else(String::new, |cost| {
                            if a { format!(" · التقدير {}", rtl::ltr(format!("${cost:.6}"))) } else { format!(" · Estimated ${cost:.6}") }
                        }),
                        if usage.has_unreported_usage {
                            text(a, " · Some usage unreported", " · بعض الاستخدام غير مبلغ")
                        } else {
                            ""
                        }
                    ),
                );
                ui.horizontal(|ui| {
                    if button(ui, a, "Cancel task — keep produced pages", "إلغاء المهمة مع الاحتفاظ بالصفحات المنتجة")
                    {
                        requests.push(AiRequest::Cancel);
                    }
                    rtl::label(ui, state.live_status_line(a));
                });
            }
            let status_message = if state.error.is_empty() { &state.snapshot.message } else { &state.error };
            if !status_message.is_empty() {
                let summary: String = status_message.lines().next().unwrap_or("").chars().take(140).collect();
                rtl::label(ui, summary);
            }
            let reviewing = state.tab == 0
                && ((state.proof_display.is_some() && state.show_saved_proof)
                    || state.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "awaiting_human_proof"));
            if reviewing {
                // The proof and its actions share a bounded, non-scrolling workspace.
                production(ui, &mut state, a, available, &documents, &mut activate_document, &mut requests);
            } else {
                egui::ScrollArea::vertical().id_salt("ai_panel_body").max_height(650.0).show(ui, |ui| match state.tab {
                    1 => skills(ui, &mut state, a, available, app.session.active().is_some(), &mut requests),
                    2 => usage(ui, &state, a),
                    3 => providers(ui, &mut state, a, available, &mut requests),
                    4 => local_ai(ui, &mut state, a, available, app.services.pick_open.is_some(), &mut requests),
                    _ => production(ui, &mut state, a, available, &documents, &mut activate_document, &mut requests),
                });
            }
            if !state.error.is_empty()
                || !state.snapshot.message.is_empty()
                || !state.snapshot.current_issues.is_empty()
                || state.snapshot.current_usage.requests > 0
            {
                egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Details", "التفاصيل"))).show(ui, |ui| {
                    egui::ScrollArea::vertical().id_salt("ai_status_details").max_height(140.0).show(ui, |ui| {
                        rtl::label(ui, &state.snapshot.message);
                        rtl::label(ui, &state.error);
                        let usage = &state.snapshot.current_usage;
                        rtl::label(
                            ui,
                            format!(
                                "{} {} · {} {} · {} {} · {} {} · {} {} · {} {}",
                                text(a, "Requests", "الطلبات"),
                                usage.requests,
                                text(a, "Input", "الإدخال"),
                                usage.input_tokens,
                                text(a, "Output", "الإخراج"),
                                usage.output_tokens,
                                text(a, "Cached", "المخزن"),
                                usage.cached_input_tokens,
                                text(a, "Reasoning", "التفكير"),
                                usage.reasoning_tokens,
                                text(a, "Estimated tokens", "الرموز المقدرة"),
                                usage.estimated_tokens
                            ),
                        );
                        if usage.has_unreported_usage {
                            label(ui, a, "Some provider usage is unreported.", "بعض بيانات الاستخدام لم يبلغها المزود.");
                        }
                        for issue in &state.snapshot.current_issues {
                            if issue.get("kind").and_then(Value::as_str) == Some("advisory_fail") {
                                let label_str = issue.get("label").and_then(Value::as_str).unwrap_or("");
                                let reasons_str = issue
                                    .get("reasons")
                                    .and_then(Value::as_array)
                                    .map(|arr| arr.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("؛ "))
                                    .unwrap_or_default();
                                rtl::label(
                                    ui,
                                    if a {
                                        format!("ملاحظة فحص بصري استشاري ({label_str} — استمر الإنتاج للمراجعة البشرية): {reasons_str}")
                                    } else {
                                        format!("Advisory visual inspection note ({label_str} — kept for human proof review): {reasons_str}")
                                    },
                                );
                            } else {
                                rtl::label(ui, serde_json::to_string_pretty(issue).unwrap_or_else(|_| issue.to_string()));
                            }
                        }
                    });
                });
            }
        });
    state.open = open;
    if ctx.input(|input| input.pointer.button_released(egui::PointerButton::Primary)) {
        state.review_pointer_press = None;
    }
    app.ui.ai = state;
    if toggle_language && let Err(error) = app.run("app.language", serde_json::json!({"lang": if a { "" } else { "ar" }})) {
        app.ui.ai.error = error;
    }
    if check_preview_update && let Err(error) = app.run("app.checkPreviewUpdate", serde_json::json!({})) {
        app.ui.ai.error = error;
    }
    if activate_document != original_document {
        if let Some(index) = activate_document
            && let Err(error) = app.run("file.activate", serde_json::json!({"index":index}))
        {
            app.ui.ai.error = error;
        }
        // A document choice is never implicit authority to start a task in that same frame.
        requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
    }
    if std::mem::take(&mut app.ui.ai.pick_ollama)
        && let Some(path) = app.services.pick_open.as_mut().and_then(|pick| pick("ollama_executable"))
    {
        app.ui.ai.ollama_executable = path;
        ctx.request_repaint();
    }
    if std::mem::take(&mut app.ui.ai.pick_target_document) {
        if app.services.pick_open.is_some() {
            if let Some(path) = app.services.pick_open.as_mut().and_then(|pick| pick("ai_target_document")) {
                app.ui.ai.open_document_path = path.clone();
                match open_target_document_path(app, &path) {
                    Ok(_) => {
                        app.ui.ai.new_document = false;
                        app.ui.ai.continue_book = false;
                        app.ui.ai.restored_target_unconfirmed = false;
                        app.ui.ai.error.clear();
                        ctx.request_repaint();
                    }
                    Err(error) => app.ui.ai.error = error,
                }
            }
        } else if let Err(error) = app.pick_and_open("ai_target_document") {
            app.ui.ai.error = error;
        }
        requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
    }
    if std::mem::take(&mut app.ui.ai.open_target_document_requested) {
        let path = app.ui.ai.open_document_path.trim().to_string();
        if path.is_empty() {
            app.ui.ai.error = text(a, "Enter a document path to open.", "أدخل مسار مستند لفتحه.").into();
        } else {
            match open_target_document_path(app, &path) {
                Ok(_) => {
                    app.ui.ai.new_document = false;
                    app.ui.ai.continue_book = false;
                    app.ui.ai.restored_target_unconfirmed = false;
                    app.ui.ai.error.clear();
                    ctx.request_repaint();
                }
                Err(error) => app.ui.ai.error = error,
            }
        }
        requests.retain(|request| !matches!(request, AiRequest::Start(_) | AiRequest::RetryBatch { .. }));
    }
    if std::mem::take(&mut app.ui.ai.polish_book_requested) {
        if let Err(error) = run_polish_current_arabic_book(app, a, &mut requests) {
            app.ui.ai.error = error;
        } else {
            app.ui.ai.error.clear();
            ctx.request_repaint();
        }
    }
    if !requests.is_empty() {
        ctx.request_repaint();
    }
    for request in requests {
        if let Err(e) = dispatch(app, request) {
            app.ui.ai.error = e;
        }
    }
    app.ui.ai.batch_approval_gesture = None;
    load_preview(app, ctx);
    load_proof_preview(app, ctx);
    load_companion_proof(app, ctx);
}

const MAX_AI_TARGET_PDF_BYTES: u64 = 64 * 1024 * 1024;

fn sanitize_pdf_page_size(size: Option<(f64, f64)>) -> (f64, f64) {
    match size {
        Some((w, h)) if w.is_finite() && h.is_finite() && (36.0..=15552.0).contains(&w) && (36.0..=15552.0).contains(&h) => (w, h),
        _ => (595.27559, 841.88976),
    }
}

fn open_target_document_path(app: &mut DesignApp, path: &str) -> Result<serde_json::Value, String> {
    let lower = path.to_ascii_lowercase();
    let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).unwrap_or("Imported");
    if [".docx", ".rtf", ".txt", ".md", ".xlsx"].iter().any(|ext| lower.ends_with(ext)) {
        let created = app.run("file.new", serde_json::json!({"preset": "A4", "pages": 1, "facingPages": false, "title": stem}))?;
        match app.open_file("file.place", serde_json::json!({"path": path, "autoflow": true})) {
            Ok(v) => Ok(v),
            Err(error) => {
                if let Some(index) = created.get("index").and_then(serde_json::Value::as_u64) {
                    let _ = app.run("file.close", serde_json::json!({"index": index}));
                }
                Err(error)
            }
        }
    } else if lower.ends_with(".pdf") {
        let bytes = if let Some(read) = app.services.read.as_mut() {
            read(path)
        } else {
            designcraft_engine::io::read_capped(path, MAX_AI_TARGET_PDF_BYTES)
                .map_err(|e| format!("{path}: {e}"))
                .and_then(|b| if (b.len() as u64) > MAX_AI_TARGET_PDF_BYTES { Err("PDF file exceeds the 64-MiB import limit".into()) } else { Ok(b) })
        };
        let bytes = match bytes {
            Ok(b) if designcraft_render::is_pdf(&b) => b,
            Ok(_) => return app.open_file("file.open", serde_json::json!({"path": path})),
            Err(error) => return Err(error),
        };
        let pages = designcraft_render::pdf_page_count(&bytes).unwrap_or(1).clamp(1, 500);
        let (w, h) = sanitize_pdf_page_size(designcraft_render::pdf_page_size(&bytes, 0));
        let created = app.run("file.new", serde_json::json!({"width": w, "height": h, "pages": pages, "facingPages": false, "title": stem}))?;
        let mut last = serde_json::Value::Null;
        for page in 1..=pages {
            match app.open_file("file.place", serde_json::json!({"path": path, "pdfPage": page, "spread": page - 1, "x": 0.0, "y": 0.0, "width": w}))
            {
                Ok(v) => last = v,
                Err(error) => {
                    if let Some(index) = created.get("index").and_then(serde_json::Value::as_u64) {
                        let _ = app.run("file.close", serde_json::json!({"index": index}));
                    }
                    return Err(error);
                }
            }
        }
        Ok(last)
    } else {
        app.open_file("file.open", serde_json::json!({"path": path}))
    }
}

pub(crate) fn open_target_document_bytes(app: &mut DesignApp, name: &str, bytes: &[u8]) -> Result<serde_json::Value, String> {
    let lower = name.to_ascii_lowercase();
    let stem = std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty()).unwrap_or("Imported");
    let b64 = designcraft_engine::cmd::base64_encode(bytes);
    let result = if [".docx", ".rtf", ".txt", ".md", ".xlsx"].iter().any(|ext| lower.ends_with(ext)) {
        let created = app.run("file.new", serde_json::json!({"preset": "A4", "pages": 1, "facingPages": false, "title": stem}))?;
        match app.open_file("file.place", serde_json::json!({"name": name, "base64": b64, "autoflow": true})) {
            Ok(v) => Ok(v),
            Err(error) => {
                if let Some(index) = created.get("index").and_then(serde_json::Value::as_u64) {
                    let _ = app.run("file.close", serde_json::json!({"index": index}));
                }
                Err(error)
            }
        }
    } else if lower.ends_with(".pdf") && designcraft_render::is_pdf(bytes) {
        if (bytes.len() as u64) > MAX_AI_TARGET_PDF_BYTES {
            return Err("PDF file exceeds the 64-MiB import limit".into());
        }
        let pages = designcraft_render::pdf_page_count(bytes).unwrap_or(1).clamp(1, 500);
        let (w, h) = sanitize_pdf_page_size(designcraft_render::pdf_page_size(bytes, 0));
        let created = app.run("file.new", serde_json::json!({"width": w, "height": h, "pages": pages, "facingPages": false, "title": stem}))?;
        let mut last = serde_json::Value::Null;
        let mut shared_asset: Option<u64> = None;
        for page in 1..=pages {
            let params = if let Some(aid) = shared_asset {
                serde_json::json!({"name": name, "reuseAsset": aid, "pdfPage": page, "spread": page - 1, "x": 0.0, "y": 0.0, "width": w})
            } else {
                serde_json::json!({"name": name, "base64": b64, "pdfPage": page, "spread": page - 1, "x": 0.0, "y": 0.0, "width": w})
            };
            match app.open_file("file.place", params) {
                Ok(v) => {
                    if shared_asset.is_none() {
                        shared_asset = v.get("asset").and_then(serde_json::Value::as_u64);
                    }
                    last = v;
                }
                Err(error) => {
                    if let Some(index) = created.get("index").and_then(serde_json::Value::as_u64) {
                        let _ = app.run("file.close", serde_json::json!({"index": index}));
                    }
                    return Err(error);
                }
            }
        }
        Ok(last)
    } else {
        app.open_file("file.openBytes", serde_json::json!({"name": stem, "base64": b64}))
    }?;
    app.ui.ai.open_document_path = name.to_string();
    app.ui.ai.new_document = false;
    app.ui.ai.continue_book = false;
    app.ui.ai.restored_target_unconfirmed = false;
    app.ui.ai.error.clear();
    Ok(result)
}

fn providers(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, available: bool, requests: &mut Vec<AiRequest>) {
    ui.add_enabled_ui(available && !s.task_active(), |ui| {
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Models and credentials", "النماذج ومفاتيح الاتصال"))).default_open(true).show(ui, |ui| {
            label(ui, a, "Text and layout model", "نموذج النص والتخطيط");
            model(ui, a, "ai_text_provider", &mut s.settings.text_model, false, &mut s.pending_models, &s.discovered_models, requests);
            label(ui, a, "Image model (separate)", "نموذج الصور (مستقل)");
            model(ui, a, "ai_image_provider", &mut s.settings.image_model, true, &mut s.pending_models, &s.discovered_models, requests);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut s.settings.output_mode, OutputMode::NativeOnly, rtl::widget(ui, text(a, "Native only", "عناصر أصلية فقط")));
                ui.selectable_value(
                    &mut s.settings.output_mode,
                    OutputMode::NativeWithImages,
                    rtl::widget(ui, text(a, "Native with images", "عناصر أصلية مع صور")),
                );
            });
            if s.settings.image_model.provider == ProviderKind::ComfyUi && s.settings.output_mode == OutputMode::NativeWithImages {
                label(ui, a, "ComfyUI workflow (API JSON)", "مخطط ComfyUI بصيغة API JSON");
                label(
                    ui,
                    a,
                    "Use {{prompt}} and {{model}} inside string values; each task fills in its own prompt and model.",
                    "ضع {{prompt}} و{{model}} داخل القيم النصية؛ تُستبدل بتعليمات المهمة ونموذجها.",
                );
                if ui.add(egui::TextEdit::multiline(&mut s.workflow_json).desired_rows(3).desired_width(f32::INFINITY)).changed() {
                    s.edit_workflow();
                }
                if !s.workflow_error.is_empty() {
                    label(
                        ui,
                        a,
                        "Complete valid JSON before applying settings or starting image production.",
                        "أكمل JSON صالحاً قبل تطبيق الإعدادات أو بدء إنتاج الصور.",
                    );
                    rtl::label(ui, &s.workflow_error);
                }
            }
            ui.separator();
            label(
                ui,
                a,
                "Credentials — saved with operating-system protection; stored secrets are never displayed",
                "المفاتيح محفوظة بحماية نظام التشغيل ولا تُعرض بعد الحفظ",
            );
            ui.horizontal(|ui| {
                ui.radio_value(&mut s.key_for_images, false, rtl::widget(ui, text(a, "Text provider", "مزود النص")));
                ui.radio_value(&mut s.key_for_images, true, rtl::widget(ui, text(a, "Image provider", "مزود الصور")));
            });
            let selection = if s.key_for_images { s.settings.image_model.clone() } else { s.settings.text_model.clone() };
            rtl::label(ui, format!("{} / {}", provider_name(selection.provider), selection.endpoint.as_deref().unwrap_or("default endpoint")));
            ui.add(egui::TextEdit::singleline(&mut s.api_key.0).password(true).char_limit(4096).desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                let remote = matches!(selection.provider, ProviderKind::OpenAi | ProviderKind::Gemini);
                if ui
                    .add_enabled(
                        remote && !s.api_key.0.trim().is_empty(),
                        egui::Button::new(rtl::widget(ui, text(a, "Save protected key", "حفظ المفتاح المحمي"))),
                    )
                    .clicked()
                {
                    if s.workflow_ready() {
                        requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                    }
                    requests.push(AiRequest::SaveApiKey { selection: selection.clone(), key: std::mem::take(&mut s.api_key.0).into() });
                }
                if button(ui, a, "Refresh presence", "تحديث حالة المفاتيح") {
                    requests.push(AiRequest::RefreshCredentials);
                }
                if ui.add_enabled(remote, egui::Button::new(rtl::widget(ui, text(a, "Remove stored key", "حذف المفتاح المحفوظ")))).clicked()
                {
                    requests.push(AiRequest::RemoveApiKey { selection: selection.clone() });
                }
            });
            if s.snapshot.credentials.is_empty() {
                label(
                    ui,
                    a,
                    "Refresh presence to check protected storage and environment credentials.",
                    "حدّث الحالة لفحص المفاتيح المحمية ومتغيرات البيئة.",
                );
            }
            for credential in &s.snapshot.credentials {
                rtl::label(
                    ui,
                    format!(
                        "{} / {}",
                        provider_name(credential.selection.provider),
                        credential.selection.endpoint.as_deref().unwrap_or("default endpoint")
                    ),
                );
                label(
                    ui,
                    a,
                    if credential.persisted { "OS-protected key saved" } else { "No protected key saved" },
                    if credential.persisted { "مفتاح محمي محفوظ" } else { "لا يوجد مفتاح محمي محفوظ" },
                );
                label(
                    ui,
                    a,
                    if credential.available { "Credential available" } else { "Credential unavailable" },
                    if credential.available { "مفتاح الاتصال متاح" } else { "مفتاح الاتصال غير متاح" },
                );
                label(
                    ui,
                    a,
                    if credential.protected_storage_available { "OS-protected storage available" } else { "OS-protected storage unavailable" },
                    if credential.protected_storage_available {
                        "مخزن حماية نظام التشغيل متاح"
                    } else {
                        "مخزن حماية نظام التشغيل غير متاح"
                    },
                );
                rtl::label(ui, &credential.message);
            }
            label(
                ui,
                a,
                "Environment credentials are also supported. A custom endpoint has its own credential entry.",
                "يمكن استخدام متغيرات البيئة أيضاً. لكل عنوان خدمة مخصص مفتاح مستقل.",
            );
            if ui.add_enabled(s.workflow_ready(), egui::Button::new(rtl::widget(ui, text(a, "Apply settings", "تطبيق الإعدادات")))).clicked()
            {
                requests.push(AiRequest::UpdateSettings(s.settings.clone()));
            }
        });
    });
}

fn local_ai(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, available: bool, can_pick: bool, requests: &mut Vec<AiRequest>) {
    label(ui, a, "Local inference services", "خدمات الذكاء الاصطناعي المحلية");
    label(
        ui,
        a,
        "Connect to installed services, check the reported models, then select one for production.",
        "اتصل بالخدمات المثبتة وافحص النماذج المتاحة ثم اختر نموذجاً للإنتاج.",
    );
    ui.add_enabled_ui(available && !s.task_active(), |ui| {
        label(ui, a, "Owned Ollama process", "عملية Ollama التي يشغلها التطبيق");
        field(ui, a, "Installed ollama.exe (absolute path)", "مسار ollama.exe المثبت (مسار كامل)", &mut s.ollama_executable);
        ui.horizontal(|ui| {
            if ui.add_enabled(can_pick, egui::Button::new(rtl::widget(ui, text(a, "Choose executable", "اختيار الملف التنفيذي")))).clicked() { s.pick_ollama = true; }
            let owned_running = s.snapshot.local_server.as_ref().is_some_and(|server| server.owned && server.running);
            if ui.add_enabled(!owned_running && std::path::Path::new(&s.ollama_executable).is_absolute(), egui::Button::new(rtl::widget(ui, text(a, "Start selected Ollama", "تشغيل Ollama المحدد")))).clicked() {
                s.local_ollama.endpoint = Some("http://127.0.0.1:11434".into());
                if s.settings.text_model.provider == ProviderKind::Ollama {
                    s.settings.text_model.endpoint = s.local_ollama.endpoint.clone();
                    if s.workflow_ready() {
                        requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                    }
                }
                requests.push(AiRequest::StartLocalOllama { executable: s.ollama_executable.clone() });
            }
            if ui.add_enabled(owned_running, egui::Button::new(rtl::widget(ui, text(a, "Stop owned process", "إيقاف العملية التابعة للتطبيق")))).clicked() { requests.push(AiRequest::StopLocalOllama); }
        });
        label(ui, a, "Starts the selected installed server on 127.0.0.1:11434. It does not install or download models; other running services are left alone.", "يشغّل الخادم المثبت المحدد على 127.0.0.1:11434. لا يثبت أو ينزل نماذج ولا يوقف الخدمات الأخرى.");
        if let Some(server) = &s.snapshot.local_server {
            let status_label = text(a, if server.running { "Running" } else { "Stopped" }, if server.running { "قيد التشغيل" } else { "متوقف" });
            rtl::label(
                ui,
                if a {
                    rtl::rtl_paragraph(format!("{} · {status_label} · {}", rtl::ltr(&server.endpoint), rtl::ltr(format!("PID {:?}", server.pid))))
                } else {
                    format!("{} · {status_label} · PID {:?}", server.endpoint, server.pid)
                },
            );
            rtl::label(ui, &server.message);
        }

        for images in [false, true] {
            let selection = if images { &mut s.local_comfy } else { &mut s.local_ollama };
            ui.separator();
            rtl::label(ui, provider_name(selection.provider));
            let mut endpoint = selection.endpoint.clone().unwrap_or_default();
            field(ui, a, "Service endpoint", "عنوان الخدمة", &mut endpoint);
            selection.endpoint = (!endpoint.trim().is_empty()).then_some(endpoint);
            field(ui, a, "Model name", "اسم النموذج", &mut selection.model);
            if button(ui, a, "Check connection and discover models", "فحص الاتصال واكتشاف النماذج") {
                s.pending_models = Some(selection.clone());
                requests.push(AiRequest::ListModels { selection: selection.clone() });
            }
            let matching = s.local_probe.as_ref().filter(|(probed, _, _, _)| probed.provider == selection.provider && probed.endpoint == selection.endpoint);
            if let Some((_, models, message, connected)) = matching {
                label(ui, a, if *connected { "Connected: discovery succeeded" } else { "Connection failed or cancelled" }, if *connected { "متصل: نجح اكتشاف النماذج" } else { "فشل الاتصال أو ألغي الفحص" });
                rtl::label(ui, message);
                if *connected && models.is_empty() { label(ui, a, "The service reported no available models.", "لم تبلغ الخدمة عن نماذج متاحة."); }
                if !models.is_empty() {
                    egui::ComboBox::from_id_salt(("local_model", images)).selected_text(&selection.model).show_ui(ui, |ui| {
                        for name in models.iter().filter(|name| images || !is_non_text_model(name)) { ui.selectable_value(&mut selection.model, name.clone(), name); }
                    });
                }
            } else {
                label(ui, a, "Connection has not been checked for this endpoint.", "لم يُفحص الاتصال بعنوان الخدمة هذا.");
            }
            let ready = !selection.model.trim().is_empty();
            let chosen = selection.clone();
            if ui.add_enabled(ready, egui::Button::new(rtl::widget(ui, text(a, if images { "Select for images" } else { "Select for text" }, if images { "اختيار لإنتاج الصور" } else { "اختيار لإنتاج النص" })))).clicked() {
                if images { s.settings.image_model = chosen; } else { s.settings.text_model = chosen; }
                if s.workflow_ready() {
                    requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                }
            }
            if images {
                label(ui, a, "Start your installed ComfyUI launcher, confirm its endpoint, then check connection. Configure API workflow in Providers and keys.", "شغّل ComfyUI بواسطة مشغّله المثبت وتحقق من عنوانه ثم افحص الاتصال. اضبط مخطط API في صفحة المزودين والمفاتيح.");
            } else {
                label(ui, a, "Start your installed Ollama application or its configured service, then check connection. Model installation is managed by Ollama.", "شغّل تطبيق Ollama المثبت أو خدمته المضبوطة ثم افحص الاتصال. يتم تثبيت النماذج بواسطة Ollama.");
            }
        }
        ui.separator();
        label(ui, a, "Resource profile", "ملف حدود الموارد");
        ui.horizontal(|ui| {
            if button(ui, a, "Conservative CPU (Ollama)", "معالج محافظ (Ollama)") {
                s.settings.resources = designcraft_ai_types::LocalResourcePolicy { cpu_threads: 2, context_tokens: 4096, batch_tokens: 32, use_gpu: false, min_free_ram_mb: 2048, max_memory_percent: 75, max_image_edge: 1024, keep_alive_seconds: 0 };
                s.settings.budget.max_output_tokens = s.settings.budget.max_output_tokens.min(2048);
            }
            if button(ui, a, "Standard limits", "حدود قياسية") { s.settings.resources = designcraft_ai_types::LocalResourcePolicy::default(); }
        });
        label(ui, a, "Conservative preset: 4096 context tokens and up to 2048 output tokens. Your review page count stays unchanged.", "الإعداد المحافظ: سياق 4096 رمزاً وإخراج حتى 2048 رمزاً. يبقى عدد صفحات مراجعتك كما اخترته.");
        let resource = &mut s.settings.resources;
        egui::Grid::new("ai_resources").num_columns(2).show(ui, |ui| {
            label(ui, a, "Ollama CPU threads", "خيوط المعالج لـOllama"); ui.add(egui::DragValue::new(&mut resource.cpu_threads).range(1..=64)); ui.end_row();
            label(ui, a, "Ollama context tokens", "رموز سياق Ollama"); ui.add(egui::DragValue::new(&mut resource.context_tokens).range(2048..=65_536)); ui.end_row();
            label(ui, a, "Ollama inference batch tokens", "رموز دفعة استدلال Ollama"); ui.add(egui::DragValue::new(&mut resource.batch_tokens).range(16..=512)); ui.end_row();
            label(ui, a, "Local minimum free RAM (MiB)", "أقل ذاكرة RAM حرة للخدمات المحلية (MiB)"); ui.add(egui::DragValue::new(&mut resource.min_free_ram_mb).range(512..=131_072)); ui.end_row();
            label(ui, a, "Local memory admission ceiling (%)", "سقف قبول الذاكرة للخدمات المحلية (%)"); ui.add(egui::DragValue::new(&mut resource.max_memory_percent).range(50..=95)); ui.end_row();
            label(ui, a, "Local maximum image edge (px)", "أقصى ضلع للصور المحلية (بكسل)"); ui.add(egui::DragValue::new(&mut resource.max_image_edge).range(256..=4096)); ui.end_row();
            label(ui, a, "Ollama keep model loaded (seconds)", "إبقاء نموذج Ollama محملاً (ثوانٍ)"); ui.add(egui::DragValue::new(&mut resource.keep_alive_seconds).range(0..=300)); ui.end_row();
        });
        ui.checkbox(&mut resource.use_gpu, rtl::widget(ui, text(a, "Allow GPU for Ollama", "السماح بالمعالج الرسومي لـOllama")));
        label(ui, a, "RAM admission and image bounds apply to local requests. ComfyUI selects CPU/GPU at service startup; Ollama controls do not limit its GPU memory.", "تُطبق حدود قبول RAM والصور على الطلبات المحلية. يختار ComfyUI المعالج أو GPU عند تشغيل خدمته؛ ولا تحد إعدادات Ollama من ذاكرته الرسومية.");
        ui.horizontal(|ui| {
            if ui.add_enabled(s.workflow_ready(), egui::Button::new(rtl::widget(ui, text(a, "Apply models and limits", "تطبيق النماذج والحدود")))).clicked() { requests.push(AiRequest::UpdateSettings(s.settings.clone())); }
            if button(ui, a, "Refresh hardware report", "تحديث تقرير الجهاز") { requests.push(AiRequest::RefreshResources); }
        });
    });
    ui.separator();
    label(ui, a, "Reported hardware", "الموارد المبلغ عنها");
    if let Some(resource) = &s.snapshot.resources {
        if resource.cpu_threads_available > 0 {
            rtl::label(ui, format!("{} {}", text(a, "Available CPU threads:", "خيوط المعالج المتاحة:"), resource.cpu_threads_available));
        }
        if resource.total_ram_mb > 0 {
            rtl::label(
                ui,
                if a {
                    rtl::rtl_paragraph(format!(
                        "{}: {} حرة / إجمالي",
                        rtl::ltr("RAM"),
                        rtl::ltr(format!("{} MiB / {} MiB", resource.available_ram_mb, resource.total_ram_mb))
                    ))
                } else {
                    format!("RAM: {} MiB / {} MiB free / total", resource.available_ram_mb, resource.total_ram_mb)
                },
            );
        }
        for gpu in &resource.gpus {
            if gpu.total_vram_mb > 0 {
                rtl::label(
                    ui,
                    if a {
                        rtl::rtl_paragraph(format!(
                            "{} · {}: {} حرة / إجمالي",
                            rtl::ltr(&gpu.name),
                            rtl::ltr("VRAM"),
                            rtl::ltr(format!("{} MiB / {} MiB", gpu.free_vram_mb, gpu.total_vram_mb))
                        ))
                    } else {
                        format!("{} · VRAM: {} MiB / {} MiB free / total", gpu.name, gpu.free_vram_mb, gpu.total_vram_mb)
                    },
                );
            } else {
                rtl::label(
                    ui,
                    if a {
                        rtl::rtl_paragraph(format!("{} · بيانات ذاكرة المعالج الرسومي غير متاحة", rtl::ltr(&gpu.name)))
                    } else {
                        format!("{} · VRAM telemetry unavailable", gpu.name)
                    },
                );
            }
        }
        if resource.gpus.is_empty() {
            label(ui, a, "No GPU telemetry was reported.", "لم تُبلغ بيانات المعالج الرسومي.");
        }
        rtl::label(ui, &resource.message);
    } else {
        label(ui, a, "Hardware has not been probed. Refresh the hardware report.", "لم تُفحص موارد الجهاز. حدّث تقرير الجهاز.");
    }
}

fn same_document_path(left: &str, right: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        left.replace('\\', "/").eq_ignore_ascii_case(&right.replace('\\', "/"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        left == right
    }
}

fn saved_book_document<'a>(draft: &ProductionDraft, documents: &'a [OpenDocument], selected: Option<usize>) -> Option<&'a OpenDocument> {
    if let Some(path) = &draft.document_path {
        documents.iter().find(|document| document.path.as_ref().is_some_and(|open| same_document_path(path, open)))
    } else if let Some(uid) = draft.document_uid {
        documents.iter().find(|document| document.uid == uid)
    } else {
        // Recovered unsaved documents have no durable identity. Only the user's selected target
        // can be prepared; a matching title never selects a document or starts production.
        documents.iter().find(|document| Some(document.index) == selected)
    }
}

fn preserved_document_geometry<'a>(s: &AiUiState, documents: &'a [OpenDocument], selected: Option<usize>) -> Option<&'a OpenDocument> {
    if s.new_document {
        return None;
    }
    documents.iter().find(|document| Some(document.index) == selected && (!s.continue_book || document.facing_pages))
}

fn run_polish_current_arabic_book(app: &mut DesignApp, a: bool, requests: &mut Vec<AiRequest>) -> Result<Value, String> {
    if app.session.active().is_none() {
        return Err(text(a, "Open an Arabic document first.", "افتح مستنداً عربياً أولاً.").into());
    }
    let tashkeel = match app.ui.ai.settings.arabic_tashkeel {
        ArabicTashkeelMode::Auto | ArabicTashkeelMode::None => "none",
        ArabicTashkeelMode::Light => "light",
        ArabicTashkeelMode::Full => "keep",
    };
    let mut params = serde_json::Map::new();
    params.insert("tashkeel".into(), Value::String(tashkeel.into()));
    if let Some(font) = app.ui.ai.settings.arabic_font.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        params.insert("arabicFont".into(), Value::String(font.into()));
    } else {
        params.insert("arabicFont".into(), Value::String("auto".into()));
    }
    params.insert("fixNumbering".into(), Value::Bool(true));
    params.insert("fixRtlCells".into(), Value::Bool(true));
    params.insert("fitOverset".into(), Value::Bool(true));
    let out = app.run("book.polishArabic", Value::Object(params))?;
    let removed = out.get("diacriticsRemoved").and_then(Value::as_u64).unwrap_or(0);
    let runs = out.get("fontRunsChanged").and_then(Value::as_u64).unwrap_or(0);
    let family = out.get("arabicFamily").and_then(Value::as_str).unwrap_or("");
    let units = out.get("unitBadgesFixed").and_then(Value::as_u64).unwrap_or(0);
    let footers = out.get("footersFixed").and_then(Value::as_u64).unwrap_or(0);
    let cards = out.get("assetsPermuted").and_then(Value::as_u64).unwrap_or(0);
    let fitted = out.get("oversetFitted").and_then(Value::as_u64).unwrap_or(0);
    app.ui.ai.polish_book_status = if a {
        format!(
            "تم تحسين الكتاب بخطوة تراجع واحدة: حُذفت {removed} حركة تشكيل · الخط '{family}' ({runs} مقطع) · الوحدات {units} · أرقام الصفحات {footers} · مطابقة البطاقات {cards} · معالجة الفائض {fitted}."
        )
    } else {
        format!(
            "Polished book in one undo step: removed {removed} diacritics · font '{family}' ({runs} runs) · unit badges {units} · footers {footers} · RTL card strips {cards} · overset fitted {fitted}."
        )
    };
    let active_uid = app.session.active().map(|d| d.uid);
    if let Some(batch) = app.ui.ai.snapshot.batch.as_ref()
        && batch.status == "awaiting_human_proof"
        && batch.document_uid == active_uid
    {
        requests.push(AiRequest::RefreshBatchProofs { job_id: batch.job_id.clone() });
    }
    Ok(out)
}

fn arabic_typography_and_polish_bar(
    ui: &mut egui::Ui,
    s: &mut AiUiState,
    a: bool,
    documents: &[OpenDocument],
    selected: Option<usize>,
    requests: &mut Vec<AiRequest>,
) {
    let mut settings_changed = false;
    ui.horizontal_wrapped(|ui| {
        label(ui, a, "Arabic diacritics (tashkeel)", "التشكيل العربي");
        for (mode, en, ar) in [
            (ArabicTashkeelMode::Auto, "Automatic (plain for children)", "تلقائي (بدون تشكيل كثيف للأطفال)"),
            (ArabicTashkeelMode::None, "No diacritics", "بدون تشكيل"),
            (ArabicTashkeelMode::Light, "Light (shadda & tanween)", "تشكيل خفيف"),
            (ArabicTashkeelMode::Full, "Full vocalization", "تشكيل كامل"),
        ] {
            if ui.selectable_label(s.settings.arabic_tashkeel == mode, rtl::widget(ui, text(a, en, ar))).clicked()
                && s.settings.arabic_tashkeel != mode
            {
                s.settings.arabic_tashkeel = mode;
                settings_changed = true;
            }
        }
    });
    let available_fonts = designcraft_fonts::FontDb::global().scoped(0).available_arabic_families();
    let current_font_label = s
        .settings
        .arabic_font
        .as_deref()
        .filter(|f| !f.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| text(a, "Automatic (best installed Arabic font)", "تلقائي (أفضل خط عربي مثبت)").to_owned());
    ui.horizontal_wrapped(|ui| {
        label(ui, a, "Arabic font", "الخط العربي");
        egui::ComboBox::from_id_salt("ai_arabic_font").selected_text(rtl::widget(ui, &current_font_label)).show_ui(ui, |ui| {
            if ui
                .selectable_label(
                    s.settings.arabic_font.is_none(),
                    rtl::widget(ui, text(a, "Automatic (best installed Arabic font)", "تلقائي (أفضل خط عربي مثبت)")),
                )
                .clicked()
                && s.settings.arabic_font.is_some()
            {
                s.settings.arabic_font = None;
                settings_changed = true;
            }
            for family in &available_fonts {
                let is_sel = s.settings.arabic_font.as_deref().is_some_and(|cur| cur.eq_ignore_ascii_case(family));
                if ui.selectable_label(is_sel, rtl::widget(ui, family)).clicked() && !is_sel {
                    s.settings.arabic_font = Some(family.clone());
                    settings_changed = true;
                }
            }
        });
        if let Some(open_doc) = documents.iter().find(|d| Some(d.index) == selected) {
            let btn_label = if a {
                format!("تطبيق التحسينات على الكتاب الحالي بالكامل (١–{})", open_doc.pages)
            } else {
                format!("Polish entire Arabic book (1–{})", open_doc.pages)
            };
            if ui.add_enabled(!s.snapshot.busy, egui::Button::new(rtl::widget(ui, &btn_label))).clicked() {
                s.polish_book_requested = true;
            }
        }
    });
    if !s.polish_book_status.is_empty() {
        rtl::label(ui, &s.polish_book_status);
    }
    if settings_changed {
        requests.push(AiRequest::UpdateSettings(s.settings.clone()));
    }
}

fn production_binding(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, documents: &[OpenDocument], selected: Option<usize>) {
    let fixed_document = preserved_document_geometry(s, documents, selected).or_else(|| {
        if !s.new_document && s.continue_book && !s.settings.facing_pages {
            documents.iter().find(|document| Some(document.index) == selected && !document.facing_pages && document.remaining_pages < document.pages)
        } else {
            None
        }
    });
    if let Some(document) = fixed_document {
        let binding = text(
            a,
            if document.right_to_left_binding { "Right to left (RTL)" } else { "Left to right (LTR)" },
            if document.right_to_left_binding {
                "من اليمين إلى اليسار (RTL)"
            } else {
                "من اليسار إلى اليمين (LTR)"
            },
        );
        if !document.facing_pages {
            label(ui, a, "Existing document uses single pages.", "يستخدم المستند الحالي صفحات منفردة.");
        }
        rtl::label(ui, format!("{} {binding}", text(a, "Existing book binding:", "اتجاه تجليد الكتاب الحالي:")));
    } else {
        ui.horizontal_wrapped(|ui| {
            label(ui, a, "Book binding", "اتجاه التجليد");
            for (value, en, ar) in [
                (None, "Automatic (content language)", "تلقائي حسب لغة المحتوى"),
                (Some(true), "Right to left (RTL)", "من اليمين إلى اليسار (RTL)"),
                (Some(false), "Left to right (LTR)", "من اليسار إلى اليمين (LTR)"),
            ] {
                ui.radio_value(&mut s.settings.right_to_left_binding, value, rtl::widget(ui, text(a, en, ar)));
            }
        });
        if s.settings.right_to_left_binding.is_none() {
            if s.settings.facing_pages {
                label(
                    ui,
                    a,
                    "Automatic binding: Arabic reads right to left; English reads left to right. The cover stays single.",
                    "اتجاه التجليد تلقائي: العربية من اليمين إلى اليسار والإنجليزية من اليسار إلى اليمين. يبقى الغلاف منفرداً.",
                );
            } else {
                label(
                    ui,
                    a,
                    "Automatic binding: Arabic sets RTL book direction and PDF reading order; English sets LTR.",
                    "اتجاه التجليد تلقائي: العربية تضبط اتجاه الكتاب وتصدير PDF من اليمين إلى اليسار، والإنجليزية من اليسار إلى اليمين.",
                );
            }
        }
    }
}

fn matching_retryable_failed_batch<'a>(s: &'a AiUiState, documents: &[OpenDocument], selected: Option<usize>) -> Option<&'a BatchSnapshot> {
    if s.snapshot.busy {
        return None;
    }
    let (Some(batch), Some(draft)) = (&s.snapshot.batch, &s.snapshot.production_draft) else {
        return None;
    };
    if batch.status != "failed" || !batch.retryable || batch.job_id != draft.id {
        return None;
    }
    let unpublished_new_book = matches!(draft.request.target, TaskTarget::NewA4 { .. })
        && batch.completed_pages == 0
        && batch.document_uid.is_none()
        && draft.document_uid.is_none();
    let bound_book_matches = documents
        .iter()
        .any(|document| Some(document.index) == selected && Some(document.uid) == batch.document_uid && Some(document.uid) == draft.document_uid);
    (unpublished_new_book || bound_book_matches).then_some(batch)
}

fn retryable_consumed_usage(s: &AiUiState, documents: &[OpenDocument], selected: Option<usize>) -> (u32, u64) {
    let Some(batch) = matching_retryable_failed_batch(s, documents, selected) else {
        return (0, 0);
    };
    let consumed_tokens = batch
        .cumulative_usage
        .input_tokens
        .saturating_add(batch.cumulative_usage.output_tokens)
        .saturating_add(batch.cumulative_usage.estimated_tokens);
    (batch.cumulative_usage.requests, consumed_tokens)
}

#[derive(Clone, Copy)]
struct BudgetTargets {
    consumed_requests: u32,
    book_requests: u32,
    review_requests: u32,
    min_text_requests: u32,
    book_tokens: Option<u64>,
    review_tokens: Option<u64>,
}

fn budget_targets_for_estimate(
    s: &AiUiState,
    documents: &[OpenDocument],
    selected: Option<usize>,
    estimate: &RequestEstimate,
    tokens: Option<TokenEstimate>,
) -> BudgetTargets {
    let (consumed_requests, consumed_tokens) = retryable_consumed_usage(s, documents, selected);
    let active_batch = matching_retryable_failed_batch(s, documents, selected);
    let retry_remaining_pages = active_batch
        .filter(|batch| batch.total_pages > batch.completed_pages)
        .map_or(estimate.remaining_pages, |batch| batch.total_pages.saturating_sub(batch.completed_pages));
    let retry_review_pages = active_batch.filter(|batch| batch.pages_per_batch > 0).map_or_else(
        || retry_remaining_pages.min(estimate.review_pages),
        |batch| {
            let step = if batch.spread_aligned && batch.completed_pages == 0 { 1 } else { batch.pages_per_batch.max(1) };
            retry_remaining_pages.min(step)
        },
    );
    let per_page_requests = estimate.allowance().checked_div(estimate.review_pages).unwrap_or(1);

    let book_requests = consumed_requests.saturating_add(retry_remaining_pages.saturating_mul(per_page_requests)).max(estimate.book_allowance);
    let review_requests = consumed_requests.saturating_add(retry_review_pages.saturating_mul(per_page_requests)).max(estimate.allowance());
    let min_text_requests = consumed_requests.saturating_add(retry_review_pages).max(estimate.review_pages);

    let book_tokens = tokens.map(|t| {
        let retry_book_tokens = t.per_page_tokens.saturating_mul(u64::from(retry_remaining_pages)).clamp(256, MAX_TOKEN_LIMIT);
        consumed_tokens.saturating_add(retry_book_tokens).max(t.book_tokens).clamp(256, MAX_TOKEN_LIMIT)
    });
    let review_tokens = tokens.map(|t| {
        let retry_review_tokens = t.per_page_tokens.saturating_mul(u64::from(retry_review_pages)).clamp(256, MAX_TOKEN_LIMIT);
        consumed_tokens.saturating_add(retry_review_tokens).max(t.review_tokens).clamp(256, MAX_TOKEN_LIMIT)
    });

    BudgetTargets { consumed_requests, book_requests, review_requests, min_text_requests, book_tokens, review_tokens }
}

fn budget_targets(s: &AiUiState, documents: &[OpenDocument], selected: Option<usize>) -> Option<BudgetTargets> {
    let estimate = s.request_estimate(documents, selected)?;
    let tokens = s.token_estimate(documents, selected);
    Some(budget_targets_for_estimate(s, documents, selected, &estimate, tokens))
}

fn production_recovery(
    ui: &mut egui::Ui,
    s: &mut AiUiState,
    a: bool,
    available: bool,
    documents: &[OpenDocument],
    activate_document: &mut Option<usize>,
    requests: &mut Vec<AiRequest>,
) {
    if let Some(batch) = &s.snapshot.batch
        && !s.snapshot.busy
        && (batch.status == "awaiting_human_proof" || (batch.status == "failed" && batch.retryable))
        && let Some(document) = documents.iter().find(|document| Some(document.uid) == batch.document_uid)
        && Some(document.index) != *activate_document
        && button(ui, a, "Open proof document", "فتح مستند البروفة")
    {
        *activate_document = Some(document.index);
    }
    if s.task_active() {
        return;
    }
    if let Some(draft) = &s.snapshot.production_draft
        && (matches!(draft.request.target, TaskTarget::ContinueBook)
            || (matches!(draft.request.target, TaskTarget::NewA4 { .. }) && (draft.document_uid.is_some() || draft.document_path.is_some())))
    {
        let selected = saved_book_document(draft, documents, *activate_document).filter(|document| document.continuable);
        ui.horizontal_wrapped(|ui| {
            rtl::label(ui, format!("{} {}", text(a, "Saved book instructions:", "تعليمات الكتاب المحفوظة:"), draft.document_title));
            if let Some(document) = selected {
                rtl::label(ui, format!("{} {}", text(a, "Target:", "المستهدف:"), document.title));
            }
        });
        if ui
            .add_enabled(
                available && selected.is_some(),
                egui::Button::new(rtl::widget(ui, text(a, "Prepare saved book continuation", "تجهيز استكمال الكتاب المحفوظ"))),
            )
            .clicked()
            && let Some(document) = selected
        {
            let request = draft.request.clone();
            let target_total_pages = draft.progress.target_total_pages;
            s.load_production_request(&request);
            if let Some(pages) = target_total_pages {
                s.pages = pages;
            }
            s.new_document = false;
            s.continue_book = true;
            *activate_document = Some(document.index);
        }
        if selected.is_none() {
            label(ui, a, "Open and select the saved book with remaining blank pages.", "افتح الكتاب المحفوظ واختره مع وجود صفحات متبقية فارغة.");
        }
    }
    if let (Some(batch), Some(draft)) = (&s.snapshot.batch, &s.snapshot.production_draft)
        && batch.status == "failed"
        && batch.retryable
        && batch.job_id == draft.id
    {
        if batch.completed_pages == 0 {
            label(
                ui,
                a,
                "Adjust limits or settings below, then Retry this group or start fresh.",
                "ارفع الحدود أو عدّل الإعدادات، ثم أعد محاولة هذه المجموعة أو ابدأ إنتاجاً جديداً.",
            );
        } else {
            label(
                ui,
                a,
                "Retry keeps cumulative usage, pinned style, and pagination while applying updated budget limits and compatible model selections. To change the brief, style, or batch size, prepare Continue and press Produce.",
                "تحتفظ إعادة المحاولة بالاستخدام التراكمي والنمط المثبّت وتقسيم الدفعات مع تطبيق حدود الميزانية والنماذج المتوافقة المحدّثة. لتغيير التعليمات أو النمط أو حجم الدفعة، جهّز الاستكمال واضغط إنتاج.",
            );
        }
        let target_matches = matching_retryable_failed_batch(s, documents, *activate_document).is_some();
        if ui
            .add_enabled(
                available && target_matches,
                egui::Button::new(rtl::widget(ui, text(a, "Retry failed review group", "إعادة محاولة مجموعة المراجعة المتعثرة"))),
            )
            .clicked()
        {
            if s.workflow_ready() {
                requests.push(AiRequest::UpdateSettings(s.settings.clone()));
            }
            requests.push(AiRequest::RetryBatch { job_id: batch.job_id.clone() });
        }
    }
    if s.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "failed" && batch.completed_pages == 0) && !s.snapshot.busy {
        let can_start_fresh = production_blocking_issue(s, available, documents, *activate_document, a).is_none();
        if ui
            .add_enabled(
                can_start_fresh,
                egui::Button::new(rtl::widget(ui, text(a, "Start fresh with current settings", "بدء إنتاج جديد بالإعدادات الحالية"))),
            )
            .clicked()
        {
            s.clear_failure_alert();
            requests.push(AiRequest::Start(s.production_task()));
        }
    }
}

fn render_diagnostic_alert_banner(
    ui: &mut egui::Ui,
    s: &mut AiUiState,
    a: bool,
    documents: &[OpenDocument],
    activate_document: Option<usize>,
    requests: &mut Vec<AiRequest>,
) {
    let Some(alert) = s.active_diagnostic_alert() else {
        return;
    };
    let tokens = crate::theme::Tokens::get(ui.ctx());
    let fill = tokens.panel_darker;
    let title_color = ui.visuals().error_fg_color;
    let accent_color = ui.visuals().warn_fg_color;
    let border = egui::Stroke::new(2.0, title_color);
    egui::Frame::group(ui.style()).fill(fill).stroke(border).corner_radius(egui::CornerRadius::same(8)).inner_margin(egui::Margin::same(10)).show(
        ui,
        |ui| {
            if a {
                rtl::label(ui, egui::RichText::new(format!("⚠ {}", alert.title_ar)).color(title_color).strong().size(14.0));
                ui.add_space(4.0);
                rtl::label(ui, egui::RichText::new(format!("السبب: {}", alert.cause_ar)).color(accent_color).strong());
                rtl::label(ui, format!("الحل المقترح: {}", alert.solution_ar));
                rtl::label(ui, format!("التفاصيل التقنية: {}", rtl::ltr(&alert.raw_error)));
            } else {
                ui.label(egui::RichText::new(format!("⚠ {}", alert.title_en)).color(title_color).strong().size(13.5));
                ui.add_space(4.0);
                ui.label(egui::RichText::new(format!("Cause: {}", alert.cause_en)).color(accent_color).strong());
                ui.label(format!("Solution: {}", alert.solution_en));
                ui.label(format!("Technical detail: {}", alert.raw_error));
            }
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                match alert.kind {
                    DiagnosticAlertKind::ContextLimit => {
                        if ui.button(rtl::widget(ui, text(a, "Enable Smart Skill Compaction", "تفعيل الدمج الذكي للمهارات"))).clicked()
                        {
                            s.automatic_skills = true;
                            s.selected_skills.clear();
                            s.clear_failure_alert();
                        }
                    }
                    DiagnosticAlertKind::CostBudget => {
                        if ui.button(rtl::widget(ui, text(a, "Open Budget Settings", "فتح إعدادات التكلفة والميزانية"))).clicked()
                        {
                            s.tab = 0;
                            s.expand_advanced_settings = Some(true);
                        }
                    }
                    DiagnosticAlertKind::InvalidModel => {
                        if s.settings.text_model.provider == ProviderKind::Gemini
                            && s.settings.text_model.model.trim() != "gemini-3.8-flash"
                            && ui.button(rtl::widget(ui, text(a, "Use gemini-3.8-flash", "استخدام gemini-3.8-flash"))).clicked()
                        {
                            s.settings.text_model.model = "gemini-3.8-flash".into();
                            requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                            s.clear_failure_alert();
                        }
                        if s.settings.output_mode == OutputMode::NativeWithImages
                            && s.settings.image_model.provider == ProviderKind::Gemini
                            && s.settings.image_model.model.trim() != "gemini-3.1-flash-image"
                            && ui.button(rtl::widget(ui, text(a, "Use gemini-3.1-flash-image", "استخدام gemini-3.1-flash-image"))).clicked()
                        {
                            s.settings.image_model.model = "gemini-3.1-flash-image".into();
                            requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                            s.clear_failure_alert();
                        }
                        if ui.button(rtl::widget(ui, text(a, "Discover Available Models", "اكتشاف النماذج المتاحة"))).clicked() {
                            let target_selection = if s.settings.output_mode == OutputMode::NativeWithImages
                                && is_non_image_model(s.settings.image_model.provider, &s.settings.image_model.model)
                                && !is_non_text_model(&s.settings.text_model.model)
                            {
                                s.settings.image_model.clone()
                            } else {
                                s.settings.text_model.clone()
                            };
                            s.pending_models = Some(target_selection.clone());
                            requests.push(AiRequest::ListModels { selection: target_selection });
                        }
                        if s.settings.output_mode == OutputMode::NativeWithImages
                            && s.settings.verify_generated_images
                            && !supports_gemini_image_review(s.settings.text_model.provider, &s.settings.text_model.model)
                            && ui.button(rtl::widget(ui, text(a, "Turn off image inspection", "إيقاف فحص الصور"))).clicked()
                        {
                            s.settings.verify_generated_images = false;
                            requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                            s.clear_failure_alert();
                        }
                        if s.settings.output_mode == OutputMode::NativeWithImages
                            && (s.settings.image_model.provider == ProviderKind::Ollama
                                || is_non_image_model(s.settings.image_model.provider, &s.settings.image_model.model))
                            && ui.button(rtl::widget(ui, text(a, "Switch to Editable elements", "التحويل إلى عناصر قابلة للتحرير"))).clicked()
                        {
                            s.settings.output_mode = OutputMode::NativeOnly;
                            requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                            s.clear_failure_alert();
                        }
                        if ui.button(rtl::widget(ui, text(a, "Open Keys & Providers", "فتح المفاتيح والمزودين"))).clicked() {
                            s.tab = 3;
                        }
                    }
                    DiagnosticAlertKind::MissingCredential | DiagnosticAlertKind::ConnectionOrRateLimit => {
                        if ui.button(rtl::widget(ui, text(a, "Open Keys & Providers", "فتح المفاتيح والمزودين"))).clicked() {
                            s.tab = 3;
                        }
                    }
                    DiagnosticAlertKind::RequestBudget => {
                        if ui.button(rtl::widget(ui, text(a, "Raise Request Allowance", "رفع حصة الطلبات تلقائياً"))).clicked() {
                            let (consumed_requests, _) = retryable_consumed_usage(s, documents, activate_document);
                            let target = budget_targets(s, documents, activate_document)
                                .map(|targets| targets.book_requests.max(targets.review_requests))
                                .unwrap_or_else(|| consumed_requests.saturating_add(32))
                                .max(s.settings.budget.max_requests.saturating_mul(2))
                                .min(MAX_REQUEST_ALLOWANCE);
                            s.settings.budget.max_requests = target;
                            requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                            s.clear_failure_alert();
                        }
                    }
                    DiagnosticAlertKind::TokenBudget => {
                        if ui.button(rtl::widget(ui, text(a, "Raise Token Limit", "رفع سقف الرموز تلقائياً"))).clicked() {
                            let (_, consumed_tokens) = retryable_consumed_usage(s, documents, activate_document);
                            let estimated = budget_targets(s, documents, activate_document)
                                .and_then(|targets| targets.book_tokens.zip(targets.review_tokens).map(|(b, r)| b.max(r)))
                                .unwrap_or_else(|| consumed_tokens.saturating_add(256_000));
                            s.settings.budget.token_limit =
                                s.settings.budget.token_limit.saturating_mul(2).max(estimated).clamp(256_000, MAX_TOKEN_LIMIT);
                            requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                            s.clear_failure_alert();
                        }
                    }
                    DiagnosticAlertKind::PageCountMismatch => {
                        if let Some(script_pages) = explicit_script_pages(&s.prompt).or_else(|| natural_prompt_pages(&s.prompt))
                            && ui.button(rtl::widget(ui, text(a, "Match Page Count to Script", "مطابقة عدد الصفحات مع السكربت"))).clicked()
                        {
                            s.new_document = true;
                            s.continue_book = false;
                            s.pages = script_pages;
                            s.restored_target_unconfirmed = false;
                            s.clear_failure_alert();
                        }
                    }
                    DiagnosticAlertKind::LayoutPreflight => {
                        if (!s.settings.allow_paid_repairs || s.settings.budget.repair_attempts == 0)
                            && ui.button(rtl::widget(ui, text(a, "Enable Automatic Layout Repair", "تفعيل الإصلاح التلقائي للتخطيط"))).clicked()
                        {
                            s.settings.allow_paid_repairs = true;
                            s.settings.budget.repair_attempts = s.settings.budget.repair_attempts.max(2);
                            requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                            s.clear_failure_alert();
                        }
                    }
                    DiagnosticAlertKind::General => {}
                }
                if ui.button(rtl::widget(ui, text(a, "Dismiss Alert", "إخفاء التنبيه"))).clicked() {
                    s.clear_failure_alert();
                }
            });
        },
    );
    ui.add_space(6.0);
}

fn selectable_skill_cards(skills: &[SkillCard], include_design_profiles: bool, require_trusted_or_active: bool) -> Vec<&SkillCard> {
    let mut chosen: Vec<&SkillCard> = Vec::new();
    for card in skills.iter().filter(|c| {
        c.enabled
            && c.compatible
            && (include_design_profiles || c.design_profile.is_none())
            && (!require_trusted_or_active || c.active || c.trusted_source)
    }) {
        if let Some(existing) = chosen.iter_mut().find(|existing| existing.id == card.id) {
            if card.active || !existing.active {
                *existing = card;
            }
        } else {
            chosen.push(card);
        }
    }
    chosen
}

fn production_blocking_issue(s: &AiUiState, available: bool, documents: &[OpenDocument], active: Option<usize>, a: bool) -> Option<&'static str> {
    let script_conflict_issue = explicit_script_pages(&s.prompt).and_then(|script_pages| {
        let mismatch = if s.new_document {
            s.pages != script_pages
        } else if s.continue_book {
            documents
                .iter()
                .find(|document| Some(document.index) == active)
                .is_some_and(|document| u32::try_from(document.pages).ok() != Some(script_pages))
        } else {
            false
        };
        mismatch.then_some(text(
            a,
            "Explicit PAGE script count must match the target book page count.",
            "يجب أن يطابق عدد صفحات التعليمات الصريحة (PAGE) عدد صفحات الكتاب المستهدف.",
        ))
    });
    let continuation_issue =
        (s.continue_book && !documents.iter().any(|document| Some(document.index) == active && document.continuable)).then_some(text(
            a,
            "Select a book with completed opening pages and remaining blank pages, or choose Edit current document.",
            "اختر كتاباً فيه صفحات أولى مكتملة ثم صفحات فارغة، أو اختر تحرير المستند الحالي.",
        ));
    s.production_issue(available, active.is_some(), a).or(continuation_issue).or(script_conflict_issue)
}

fn production(
    ui: &mut egui::Ui,
    s: &mut AiUiState,
    a: bool,
    available: bool,
    documents: &[OpenDocument],
    activate_document: &mut Option<usize>,
    requests: &mut Vec<AiRequest>,
) {
    render_diagnostic_alert_banner(ui, s, a, documents, *activate_document, requests);
    production_recovery(ui, s, a, available, documents, activate_document, requests);
    if (s.proof_display.is_some() && s.show_saved_proof) || s.snapshot.batch.as_ref().is_some_and(|batch| batch.status == "awaiting_human_proof") {
        if s.proof_key.is_none() {
            label(
                ui,
                a,
                "Saved proof — view only. Restore the request to continue the opened book; this proof grants no approval.",
                "بروف محفوظ للعرض فقط. استعد الطلب لاستكمال الكتاب المفتوح؛ هذه المعاينة لا تمنح اعتماداً.",
            );
            if button(ui, a, "Return to production settings", "العودة إلى إعدادات الإنتاج") {
                s.show_saved_proof = false;
            }
        }
        batch_review(ui, s, a, requests);
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Production request", "تعليمات الإنتاج"))).show(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("ai_review_request").max_height(140.0).show(ui, |ui| {
                rtl::label(ui, &s.prompt);
            });
        });
        return;
    }
    ui.add_enabled_ui(!s.task_active(), |ui| {
        if let Some(document) = documents.iter().find(|document| Some(document.index) == *activate_document && document.continuable) {
            ui.horizontal_wrapped(|ui| {
                let label = if a { format!("استكمال الصفحات {} المتبقية", document.remaining_pages) } else { format!("Continue remaining {} pages", document.remaining_pages) };
                if ui.button(rtl::widget(ui, &label)).clicked() {
                    s.new_document = false;
                    s.continue_book = true;
                    s.restored_target_unconfirmed = false;
                }
                let completed = document.pages.saturating_sub(document.remaining_pages);
                let next_page = completed.saturating_add(1);
                rtl::label(
                    ui,
                    if a {
                        format!(
                            "{} · الصفحات المكتملة 1–{} · الصفحة التالية {} من {}",
                            document.title, completed, next_page, document.pages
                        )
                    } else {
                        format!(
                            "{} · Completed pages 1–{} · Next page {} of {}",
                            document.title, completed, next_page, document.pages
                        )
                    },
                );
            });
        }
        ui.horizontal_wrapped(|ui| {
            provider(ui, "production_text_provider", &mut s.settings.text_model.provider, false);
            label(ui, a, "Model or agent", "النموذج أو الوكيل");
            ui.add(egui::TextEdit::singleline(&mut s.settings.text_model.model).desired_width(230.0));
            if button(ui, a, "Discover", "اكتشاف") {
                s.pending_models = Some(s.settings.text_model.clone());
                requests.push(AiRequest::ListModels { selection: s.settings.text_model.clone() });
            }
            if button(ui, a, "Keys and providers", "المفاتيح والمزودون") {
                s.tab = 3;
            }
        });
        if let Some(models) = s.discovered_models.get(&(s.settings.text_model.provider, s.settings.text_model.endpoint.clone())) {
            let filtered: Vec<&String> = models.iter().filter(|name| !is_non_text_model(name)).collect();
            if !filtered.is_empty() {
                egui::ComboBox::from_id_salt("production_discovered_models")
                    .selected_text(rtl::widget(ui, text(a, "Available models", "النماذج المتاحة")))
                    .show_ui(
                    ui,
                    |ui| {
                        for name in filtered {
                            ui.selectable_value(&mut s.settings.text_model.model, name.clone(), name);
                        }
                    },
                );
            }
        }
        if s.active_diagnostic_alert().map(|alert| alert.kind) != Some(DiagnosticAlertKind::InvalidModel)
            && is_retired_gemini_text_model(s.settings.text_model.provider, &s.settings.text_model.model)
        {
            ui.horizontal_wrapped(|ui| {
                rtl::label(
                    ui,
                    egui::RichText::new(text(
                        a,
                        "Selected Gemini model series is retired and may return HTTP 404. Switch to gemini-3.8-flash before producing.",
                        "سلسلة نموذج Gemini المختارة متقاعدة وقد تعيد خطأ HTTP 404. انتقل إلى gemini-3.8-flash قبل الإنتاج.",
                    ))
                    .color(ui.visuals().warn_fg_color),
                );
                if ui.button(rtl::widget(ui, text(a, "Use gemini-3.8-flash", "استخدام gemini-3.8-flash"))).clicked() {
                    s.settings.text_model.model = "gemini-3.8-flash".into();
                    requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                    s.clear_failure_alert();
                }
            });
        }
        ui.horizontal_wrapped(|ui| {
            let prev_mode = s.settings.output_mode;
            ui.selectable_value(
                &mut s.settings.output_mode,
                OutputMode::NativeOnly,
                rtl::widget(ui, text(a, "Editable elements", "عناصر قابلة للتحرير")),
            );
            ui.selectable_value(
                &mut s.settings.output_mode,
                OutputMode::NativeWithImages,
                rtl::widget(ui, text(a, "Editable elements and images", "عناصر قابلة للتحرير وصور")),
            );
            if s.settings.output_mode != prev_mode {
                s.rates_confirmed = has_configured_rates(&s.settings);
                if s.workflow_ready() {
                    requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                }
            }
        });
        if s.settings.output_mode == OutputMode::NativeWithImages {
            model(ui, a, "production_image_provider", &mut s.settings.image_model, true, &mut s.pending_models, &s.discovered_models, requests);
            image_inspection_setting(ui, s, a);
        }
        label(ui, a, "What would you like to create?", "ما الذي تريد إنتاجه؟");
        ui.horizontal_wrapped(|ui| {
            label(ui, a, "Design style", "نمط التصميم");
            let available_styles = selectable_skill_cards(&s.snapshot.skills, true, !s.automatic_skills);
            let selected = s
                .settings
                .design_style
                .as_ref()
                .and_then(|id| {
                    available_styles
                        .iter()
                        .copied()
                        .find(|card| &card.id == id)
                        .or_else(|| s.snapshot.skills.iter().rev().find(|card| &card.id == id && card.active))
                        .or_else(|| s.snapshot.skills.iter().rev().find(|card| &card.id == id))
                })
                .and_then(|card| card.design_profile.as_ref())
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| {
                    s.settings.design_style.clone().unwrap_or_else(|| text(a, "Automatic from the request", "تلقائي حسب المطلوب").into())
                });
            egui::ComboBox::from_id_salt("production_design_style").selected_text(rtl::widget(ui, selected)).width(260.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut s.settings.design_style, None, rtl::widget(ui, text(a, "Automatic from the request", "تلقائي حسب المطلوب")));
                for card in available_styles {
                    if let Some(profile) = &card.design_profile {
                        ui.selectable_value(&mut s.settings.design_style, Some(card.id.clone()), rtl::widget(ui, &profile.name));
                    }
                }
            });
            if button(ui, a, "Style library", "مكتبة الأنماط") {
                s.tab = 1;
            }
        });
        // Keep room for the compact update row and the production action. Long
        // briefs remain complete and scroll inside their own composer.
        let prompt_rtl = if s.prompt.trim().is_empty() { a } else { rtl::has_rtl(&s.prompt) };
        egui::ScrollArea::vertical().id_salt("ai_prompt_composer").max_height(84.0).show(ui, |ui| {
            let mut layouter = bidi_layouter(true);
            rtl::show_text_edit(
                ui,
                egui::TextEdit::multiline(&mut s.prompt)
                    .desired_rows(4)
                    .desired_width(f32::INFINITY)
                    .horizontal_align(if prompt_rtl { egui::Align::Max } else { egui::Align::Min })
                    .layouter(&mut layouter)
                    .hint_text(rtl::widget(
                        ui,
                        text(
                            a,
                            "Describe your design, audience and desired result...",
                            "اكتب التصميم والفئة العمرية والنتيجة المطلوبة…",
                        ),
                    )),
            );
        });
        rtl::label(
            ui,
            if a {
                format!(
                    "طول التعليمات: {}",
                    rtl::ltr(format!("{} / {} UTF-8 bytes", s.prompt.len(), designcraft_ai_types::protocol::MAX_BRIEF_BYTES))
                )
            } else {
                format!("Prompt length: {} / {} UTF-8 bytes", s.prompt.len(), designcraft_ai_types::protocol::MAX_BRIEF_BYTES)
            },
        );
        if s.prompt.len() > 32768 {
            rtl::label(
                ui,
                format!(
                    "{} {} {}",
                    text(a, "Conservative input budget reservation starts at", "يبدأ حجز ميزانية الإدخال التحفظي من"),
                    s.prompt.len().div_ceil(3).saturating_add(1024),
                    text(
                        a,
                        "tokens; skill/document context is additional. Actual usage comes from the provider.",
                        "توكن؛ يضاف سياق المهارات والمستند. الاستخدام الفعلي يبلغه المزود."
                    )
                ),
            );
        }
        ui.horizontal_wrapped(|ui| {
            if ui.radio(!s.new_document && !s.continue_book, rtl::widget(ui, text(a, "Edit current document", "تحرير المستند الحالي"))).clicked() {
                s.new_document = false;
                s.continue_book = false;
                s.restored_target_unconfirmed = false;
            }
            if ui
                .radio(s.new_document, rtl::widget(ui, text(a, "New document (style size; A4 default)", "مستند جديد (مقاس النمط؛ الافتراضي A4)")))
                .clicked()
            {
                s.new_document = true;
                s.continue_book = false;
                s.restored_target_unconfirmed = false;
            }
            if ui.radio(s.continue_book, rtl::widget(ui, text(a, "Continue remaining book pages", "استكمال صفحات الكتاب المتبقية"))).clicked()
            {
                s.new_document = false;
                s.continue_book = true;
                s.restored_target_unconfirmed = false;
            }
            if button(ui, a, "Open file to edit…", "فتح ملف للتعديل…") {
                s.pick_target_document = true;
            }
            if s.new_document {
                if a {
                    ui.add(egui::DragValue::new(&mut s.pages).range(1..=1000));
                    label(ui, a, "pages", "صفحة");
                } else {
                    ui.add(egui::DragValue::new(&mut s.pages).range(1..=1000).suffix(" pages"));
                }
            }
            label(ui, a, "Pages per review", "الصفحات لكل مراجعة");
            ui.add_enabled(s.new_document || s.continue_book, egui::DragValue::new(&mut s.settings.pages_per_batch).range(1..=1000));
            if let Some(document) = preserved_document_geometry(s, documents, *activate_document) {
                let mut actual = document.facing_pages;
                ui.add_enabled(false, egui::Checkbox::new(&mut actual, rtl::widget(ui, text(a, "Facing pages", "صفحات متقابلة"))));
            } else {
                ui.checkbox(&mut s.settings.facing_pages, rtl::widget(ui, text(a, "Facing pages", "صفحات متقابلة")));
            }
        });
        production_binding(ui, s, a, documents, *activate_document);
        if !s.new_document {
            let selected = documents.iter().find(|document| Some(document.index) == *activate_document);
            ui.horizontal_wrapped(|ui| {
                label(ui, a, "Target document", "المستند المستهدف");
                egui::ComboBox::from_id_salt("ai_target_document")
                    .selected_text(rtl::widget(
                        ui,
                        selected.map(|document| document.title.as_str()).unwrap_or(text(a, "Open a document", "افتح مستنداً")),
                    ))
                    .show_ui(ui, |ui| {
                        for document in documents {
                            if ui.selectable_value(activate_document, Some(document.index), rtl::widget(ui, &document.title)).clicked() {
                                s.restored_target_unconfirmed = false;
                            }
                        }
                    });
                if let Some(selected) = selected {
                    rtl::label(ui, format!("{} {}", selected.pages, text(a, "pages", "صفحة")));
                    rtl::label(ui, selected.page_map_line(a));
                }
            });
            ui.horizontal_wrapped(|ui| {
                label(
                    ui,
                    a,
                    "Document file (.designcraft, .idml, .indd, .indt, .pdf, .docx, .rtf, .txt, .md, .xlsx)",
                    "ملف المستند (.designcraft أو .idml أو .indd أو .indt أو .pdf أو .docx أو .rtf أو .txt أو .md أو .xlsx)",
                );
                let mut open_path_layouter = bidi_layouter(false);
                rtl::show_text_edit(ui, egui::TextEdit::singleline(&mut s.open_document_path).desired_width(280.0).layouter(&mut open_path_layouter));
                if button(ui, a, "Open path", "فتح المسار") {
                    s.open_target_document_requested = true;
                }
                if button(ui, a, "Browse…", "استعراض…") {
                    s.pick_target_document = true;
                }
            });
            if let Some(selected) = selected
                && s.continue_book
                && let Some(reason) = selected.non_continuable_reason(a)
            {
                rtl::label(ui, reason);
            }
            if !s.continue_book {
                label(
                    ui,
                    a,
                    "Edits existing content in one task. Choose Continue remaining book pages for production in review groups.",
                    "يحرر المحتوى الموجود بمهمة واحدة. اختر استكمال صفحات الكتاب المتبقية للإنتاج في مجموعات مراجعة.",
                );
            }
        }
        if let Some(script_pages) = explicit_script_pages(&s.prompt) {
            if s.new_document && s.pages != script_pages {
                ui.horizontal_wrapped(|ui| {
                    rtl::label(
                        ui,
                        if a {
                            format!("تحدد التعليمات {script_pages} صفحة صريحة (PAGE)، بينما ضُبط المستند الجديد على {} صفحة.", s.pages)
                        } else {
                            format!("Script defines {script_pages} explicit PAGE blocks, but New document is set to {} pages.", s.pages)
                        },
                    );
                    let match_label = if a {
                        format!("مطابقة عدد صفحات المستند للتعليمات ({script_pages} صفحة)")
                    } else {
                        format!("Match document page count to script ({script_pages} pages)")
                    };
                    if ui.button(rtl::widget(ui, &match_label)).clicked() {
                        s.pages = script_pages;
                    }
                });
            } else if !s.new_document
                && let Some(selected) = documents.iter().find(|document| Some(document.index) == *activate_document)
                && u32::try_from(selected.pages).ok() != Some(script_pages)
            {
                ui.horizontal_wrapped(|ui| {
                    rtl::label(
                        ui,
                        if a {
                            format!(
                                "تحدد التعليمات {script_pages} صفحة صريحة (PAGE)، بينما يحتوي المستند المحدد '{}' على {} صفحة.",
                                selected.title, selected.pages
                            )
                        } else {
                            format!(
                                "Script defines {script_pages} explicit PAGE blocks, but selected document '{}' has {} pages.",
                                selected.title, selected.pages
                            )
                        },
                    );
                    if let Some(matching_doc) = documents
                        .iter()
                        .find(|doc| doc.index != selected.index && u32::try_from(doc.pages).ok() == Some(script_pages))
                    {
                        let switch_book_label = if a {
                            format!("التبديل إلى الكتاب المطابق ({})", matching_doc.title)
                        } else {
                            format!("Switch to matching book ({})", matching_doc.title)
                        };
                        if ui.button(rtl::widget(ui, &switch_book_label)).clicked() {
                            *activate_document = Some(matching_doc.index);
                            s.restored_target_unconfirmed = false;
                        }
                    }
                    let switch_new_label = if a {
                        format!("التبديل إلى مستند جديد من {script_pages} صفحة")
                    } else {
                        format!("Switch to new {script_pages}-page document")
                    };
                    if ui.button(rtl::widget(ui, &switch_new_label)).clicked() {
                        s.new_document = true;
                        s.continue_book = false;
                        s.pages = script_pages;
                        s.restored_target_unconfirmed = false;
                    }
                });
            }
        } else if s.new_document
            && let Some(detected) = natural_prompt_pages(&s.prompt)
            && s.pages != detected
        {
            ui.horizontal_wrapped(|ui| {
                rtl::label(
                    ui,
                    egui::RichText::new(if a {
                        format!("يذكر الوصف {detected} صفحة، بينما ضُبط المستند الجديد على {} صفحة.", s.pages)
                    } else {
                        format!("Prompt mentions {detected} pages, while New document is set to {} pages.", s.pages)
                    })
                    .color(ui.visuals().warn_fg_color),
                );
                let match_label = if a {
                    format!("مطابقة عدد صفحات المستند مع الوصف ({detected} صفحة)")
                } else {
                    format!("Match document pages to prompt ({detected} pages)")
                };
                if ui.button(rtl::widget(ui, &match_label)).clicked() {
                    s.pages = detected;
                    s.restored_target_unconfirmed = false;
                    s.clear_failure_alert();
                }
            });
        }
        if s.automatic_skills {
            label(ui, a, "Relevant skills are selected, checked and activated automatically.", "تُختار المهارات المناسبة وتُفحص وتُفعّل تلقائياً.");
        }
        if !s.snapshot.skill_warnings.is_empty() {
            egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Skipped skill drafts — details", "مسودات مهارات مستبعدة — التفاصيل"))).show(
                ui,
                |ui| {
                    for warning in &s.snapshot.skill_warnings {
                        rtl::label(ui, warning);
                    }
                },
            );
        }
        label(
            ui,
            a,
            "Template styles use the model for content; native drawing and formatting commands are assembled locally.",
            "أنماط القوالب تستخدم النموذج للمحتوى؛ أوامر الرسم والتنسيق تُركّب وتُنفّذ محلياً بالمحرك.",
        );
        if let Some(cost) = s.settings.budget.max_cost_usd {
            rtl::label(
                ui,
                if a {
                    format!("سقف كلفة المهمة (تقديري): {}", rtl::ltr(format!("${cost:.2}")))
                } else {
                    format!("Task cost ceiling (estimate): ${cost:.2}")
                },
            );
        }
        label(
            ui,
            a,
            "Your review page count stays unchanged. Internal requests build this group, then stop for your approval before the next review group.",
            "يبقى عدد صفحات مراجعتك كما اخترته. تُنفّذ المجموعة بطلبات داخلية، ثم تتوقف لاعتمادك قبل مجموعة المراجعة التالية.",
        );
        if let Some(estimate) = s.request_estimate(documents, *activate_document) {
            let tokens = s.token_estimate(documents, *activate_document);
            let targets = budget_targets_for_estimate(s, documents, *activate_document, &estimate, tokens);
            if estimate.remaining_pages > estimate.review_pages {
                rtl::label(ui, format!("{}: {} · {} {} · {} {}", text(a, "Remaining book requests (estimate, before repairs)", "طلبات بقية الكتاب (تقدير قبل الإصلاحات)"),
                    estimate.book_allowance, estimate.remaining_pages, text(a, "pages", "صفحة"), estimate.remaining_groups, text(a, "review groups", "مجموعات مراجعة")));
                let need_book_requests = s.settings.budget.max_requests < targets.book_requests;
                let need_book_tokens = targets.book_tokens.is_some_and(|target| s.settings.budget.token_limit < target);
                if need_book_requests || need_book_tokens {
                    ui.horizontal_wrapped(|ui| {
                        if need_book_requests
                            && ui
                                .add_enabled(
                                    targets.book_requests <= MAX_REQUEST_ALLOWANCE,
                                    egui::Button::new(rtl::widget(ui, text(a, "Set request allowance for remaining book", "ضبط حصة الطلبات لبقية الكتاب"))),
                                )
                                .clicked()
                        {
                            s.settings.budget.max_requests = targets.book_requests;
                            requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                        }
                        if let Some(target) = targets.book_tokens
                            && s.settings.budget.token_limit < target
                            && ui.button(rtl::widget(ui, text(a, "Set token limit for remaining book", "ضبط سقف الرموز لبقية الكتاب"))).clicked()
                        {
                            s.settings.budget.token_limit = target;
                            requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                        }
                    });
                }
            }
            rtl::label(
                ui,
                format!(
                    "{}: {}{} {} + {} {}{} = {}",
                    if estimate.scripted {
                        text(a, "Requests for this review (estimate)", "طلبات هذه المراجعة (تقدير)")
                    } else {
                        text(a, "Requests for this review (conservative estimate)", "طلبات هذه المراجعة (تقدير تحفظي)")
                    },
                    if estimate.scripted { "" } else { text(a, "up to ", "حتى ") },
                    estimate.review_pages,
                    text(a, "text", "للنص"),
                    estimate.image_allowance,
                    text(a, "images", "للصور"),
                    if estimate.vision_allowance > 0 {
                        format!(" + {} {}", estimate.vision_allowance, text(a, "vision checks", "لفحص الصور"))
                    } else {
                        String::new()
                    },
                    estimate.allowance()
                ),
            );
            label(
                ui,
                a,
                if estimate.scripted {
                    "One text request per PAGE; image allowance is provisional. Repairs and later reviews share the task limit. Token and cost limits still apply."
                } else {
                    "Conservative planning allowance: up to one text request per page before repairs. Internal grouping and cache reuse can reduce actual requests; image counts are provisional. Repairs and later reviews share the task limit. Token and cost limits still apply."
                },
                if estimate.scripted {
                    "طلب نص لكل PAGE؛ حصة الصور تقديرية. تشترك الإصلاحات والمراجعات التالية في حد المهمة. تبقى حدود التوكن والكلفة سارية."
                } else {
                    "حصة تخطيط تحفظية: حتى طلب نص لكل صفحة قبل الإصلاحات. قد يقل العدد الفعلي بالتجميع الداخلي وإعادة استخدام المخزن؛ وعدد الصور تقديري. تشترك الإصلاحات والمراجعات التالية في حد المهمة. تبقى حدود التوكن والكلفة سارية."
                },
            );
            if estimate.vision_allowance > 0 {
                label(
                    ui,
                    a,
                    "Image inspection adds one billed text request per uncached generated source; this estimate assumes no cached reuse.",
                    "يضيف فحص الصور طلب نص مدفوعاً لكل مصدر مولد غير مخزن؛ يفترض هذا التقدير عدم إعادة استخدام صور مخزنة.",
                );
            }
            if s.settings.budget.max_requests < targets.min_text_requests {
                let warning = if targets.consumed_requests == 0 {
                    format!(
                        "{} {} {} {}.",
                        text(a, "Configured request limit", "حد الطلبات المحدد"),
                        s.settings.budget.max_requests,
                        if estimate.scripted {
                            text(a, "is below the minimum text requests for this review:", "أقل من الحد الأدنى لطلبات نص هذه المراجعة:")
                        } else {
                            text(a, "is below the conservative planning allowance for this review:", "أقل من حصة التخطيط التحفظية لهذه المراجعة:")
                        },
                        targets.min_text_requests
                    )
                } else if a {
                    format!(
                        "حد الطلبات المحدد {} {} {} (استُخدم منها {}).",
                        s.settings.budget.max_requests,
                        if estimate.scripted {
                            "أقل من الحد الأدنى التراكمي لطلبات النص لإعادة محاولة هذه المراجعة:"
                        } else {
                            "أقل من الحصة التراكمية التحفظية لإعادة محاولة هذه المراجعة:"
                        },
                        targets.min_text_requests,
                        targets.consumed_requests
                    )
                } else {
                    format!(
                        "Configured request limit {} {} {} ({} already used).",
                        s.settings.budget.max_requests,
                        if estimate.scripted {
                            "is below the minimum cumulative text requests to retry this review:"
                        } else {
                            "is below the conservative cumulative allowance to retry this review:"
                        },
                        targets.min_text_requests,
                        targets.consumed_requests
                    )
                };
                rtl::label(ui, warning);
            }
            if s.settings.budget.max_requests < targets.review_requests {
                if ui
                    .add_enabled(
                        targets.review_requests <= MAX_REQUEST_ALLOWANCE,
                        egui::Button::new(rtl::widget(ui, text(a, "Set request allowance for this review", "ضبط حصة الطلبات لهذه المراجعة"))),
                    )
                    .clicked()
                {
                    s.settings.budget.max_requests = targets.review_requests;
                    requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                }
                if targets.review_requests > MAX_REQUEST_ALLOWANCE {
                    label(ui, a, "This provisional estimate exceeds the 10,000-request task ceiling.", "يتجاوز هذا التقدير الأولي سقف المهمة البالغ ١٠٬٠٠٠ طلب.");
                }
            }
            if let Some(target) = targets.review_tokens
                && s.settings.budget.token_limit < target
            {
                rtl::label(
                    ui,
                    egui::RichText::new(if a {
                        format!(
                            "سقف الرموز الحالي ({}) أقل من التقدير التحفظي لهذه المراجعة ({}).",
                            s.settings.budget.token_limit, target
                        )
                    } else {
                        format!(
                            "Configured token limit ({}) is below the conservative estimate for this review ({}).",
                            s.settings.budget.token_limit, target
                        )
                    })
                    .color(ui.visuals().warn_fg_color),
                );
                if ui
                    .button(rtl::widget(ui, text(a, "Set token limit for this review", "ضبط سقف الرموز لهذه المراجعة")))
                    .clicked()
                {
                    s.settings.budget.token_limit = target;
                    requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                }
            }
        }
        let issue = production_blocking_issue(s, available, documents, *activate_document, a);
        if let Some(reason) = issue {
            rtl::label(ui, reason);
        }
        if ui.add_enabled(issue.is_none(), egui::Button::new(rtl::widget(ui, text(a, "Produce", "إنتاج")))).clicked() {
            requests.push(AiRequest::Start(s.production_task()));
        }
        if (s.new_document || s.continue_book) && ui.add_enabled(available && !s.prompt.trim().is_empty(),
            egui::Button::new(rtl::widget(ui,text(a,"Preview curriculum locally (no API request)","معاينة خطة المنهج محلياً (دون طلب API)")))).clicked() {
            requests.push(AiRequest::PreviewProduction(s.production_task()));
        }
        ui.separator();
        arabic_typography_and_polish_bar(ui, s, a, documents, *activate_document, requests);
        if let Some(preview)=&s.snapshot.production_preview {
            egui::CollapsingHeader::new(rtl::widget(ui,text(a,"Last local curriculum preview","آخر معاينة محلية لخطة المنهج"))).default_open(true).show(ui,|ui| {
                rtl::label(ui,format!("{} · {} · {} · {} {}",preview["blueprint"]["subject_domain"].as_str().unwrap_or(""),
                    preview["style"].as_str().unwrap_or(""),preview["binding"].as_str().unwrap_or(""),preview["total_pages"],text(a,"pages","صفحة")));
                label(ui,a,"Suggested family sequence only; refresh after changing the request. Educator review is required.",
                    "تسلسل عائلات مقترح؛ أعد المعاينة بعد تغيير الطلب. يلزم فحص تربوي بشري.");
                egui::ScrollArea::vertical().id_salt("ai_curriculum_summary").max_height(100.0).show(ui,|ui| {
                    let has_ar_font = a || ui.ctx().fonts(|f| f.families().contains(&egui::FontFamily::Name("arabic".into())));
                    for page in preview["blueprint"]["scoped_page_plan"].as_array().into_iter().flatten().take(32) {
                        if let Some(number)=page["page"].as_u64() {
                            let archetype = if has_ar_font {
                                page["display_archetype"].as_str().or_else(|| page["recommended_family"].as_str()).unwrap_or("")
                            } else {
                                page["recommended_family"].as_str().unwrap_or("")
                            };
                            let lesson = if has_ar_font { page["chapter_and_lesson"].as_str().unwrap_or("") } else { "" };
                            let line = if lesson.is_empty() {
                                format!("{} {}: {}", text(a, "Page", "الصفحة"), number.saturating_add(1), archetype)
                            } else {
                                format!("{} {}: {} — {}", text(a, "Page", "الصفحة"), number.saturating_add(1), archetype, lesson)
                            };
                            rtl::label(ui, line);
                        }
                    }
                });
            });
        }
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Advanced settings", "إعدادات متقدمة"))).open(s.expand_advanced_settings.take()).show(ui, |ui| {
            budgets(ui, s, a);
            ui.separator();
            ui.checkbox(&mut s.automatic_skills, rtl::widget(ui, text(a, "Automatic skills", "المهارات التلقائية")));
            if s.automatic_skills {
                egui::CollapsingHeader::new(rtl::widget(
                    ui,
                    text(
                        a,
                        "Pin additional skills alongside automatic selection",
                        "تثبيت مهارات إضافية مع الاختيار التلقائي",
                    ),
                ))
                .show(ui, |ui| {
                    for card in selectable_skill_cards(&s.snapshot.skills, false, false) {
                        let mut selected = s.selected_skills.contains(&card.id);
                        if ui.checkbox(&mut selected, rtl::widget(ui, &card.name)).changed() {
                            if selected {
                                s.selected_skills.insert(card.id.clone());
                            } else {
                                s.selected_skills.remove(&card.id);
                            }
                        }
                    }
                });
            } else {
                for card in selectable_skill_cards(&s.snapshot.skills, true, true) {
                    let mut selected = s.selected_skills.contains(&card.id);
                    if ui.checkbox(&mut selected, rtl::widget(ui, &card.name)).changed() {
                        if selected {
                            s.selected_skills.insert(card.id.clone());
                        } else {
                            s.selected_skills.remove(&card.id);
                        }
                    }
                }
            }
            field(ui, a, "Output/sample directory (optional for production)", "مجلد المخرجات والعينة (اختياري للإنتاج)", &mut s.directory);
            if ui
                .add_enabled(
                    available && !s.directory.trim().is_empty(),
                    egui::Button::new(rtl::widget(ui, text(a, "Create original 8-page science example", "إنشاء مثال علوم أصلي من ٨ صفحات"))),
                )
                .clicked()
            {
                requests.push(AiRequest::CreateScienceSample { directory: s.directory.clone() });
            }
        });
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Save, export and print", "الحفظ والتصدير والطباعة"))).show(ui, |ui| {
            export_print(ui, s, a, available && activate_document.is_some(), requests);
        });
    });
    ui.separator();
    if s.snapshot.batch.as_ref().is_some_and(|batch| matches!(batch.status.as_str(), "running" | "awaiting_human_proof")) {
        batch_review(ui, s, a, requests);
    } else if s.snapshot.batch.is_some() {
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Previous task", "المهمة السابقة"))).show(ui, |ui| batch_review(ui, s, a, requests));
    }
}

fn elapsed(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{:02}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60)
}

fn usage_line(ui: &mut egui::Ui, a: bool, usage: &designcraft_ai_types::protocol::UsageSummary, elapsed_ms: u64) {
    rtl::label(
        ui,
        format!(
            "{} {} · {} {} · {} {} · {} {}",
            text(a, "Time", "الوقت"),
            elapsed(elapsed_ms),
            text(a, "Requests", "الطلبات"),
            usage.requests,
            text(a, "Input", "الإدخال"),
            usage.input_tokens,
            text(a, "Output", "الإخراج"),
            usage.output_tokens
        ),
    );
    rtl::label(
        ui,
        format!(
            "{} {} · {} {} · {} {}",
            text(a, "Cached", "المخزن"),
            usage.cached_input_tokens,
            text(a, "Reasoning", "التفكير"),
            usage.reasoning_tokens,
            text(a, "Estimated tokens", "الرموز المقدرة"),
            usage.estimated_tokens
        ),
    );
    if let Some(cost) = usage.estimated_cost_usd {
        rtl::label(ui, if a { format!("التكلفة التقديرية {}", rtl::ltr(format!("${cost:.6}"))) } else { format!("Estimated cost ${cost:.6}") });
    }
    if usage.has_unreported_usage {
        label(ui, a, "Some usage has not been reported by the provider.", "لم يبلغ المزود عن بعض بيانات الاستخدام.");
    }
}

fn proof_image(
    ui: &mut egui::Ui,
    a: bool,
    texture: Option<&egui::TextureHandle>,
    error: &str,
    title: &str,
    size: egui::Vec2,
) -> Option<egui::Response> {
    if let Some(texture) = texture {
        Some(ui.add(egui::Image::new(texture).fit_to_exact_size(size).alt_text(title).sense(egui::Sense::click())))
    } else {
        label(ui, a, "Page preview unavailable", "معاينة الصفحة غير متاحة");
        if !error.is_empty() {
            egui::ScrollArea::vertical().id_salt("ai_proof_error").max_height((size.y - 30.0).max(40.0)).show(ui, |ui| {
                rtl::label(ui, error);
            });
        }
        None
    }
}

fn batch_review(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, requests: &mut Vec<AiRequest>) {
    let can_review = s.can_review_selected_proof();
    let pair_visible = s.companion_proof_path().is_some();
    let can_approve = s.can_approve_batch();
    let facing_pages = s.effective_proof_facing_pages();
    let pages: Vec<_> = s.proof_display.as_ref().map_or_else(Vec::new, |key| key.pages().collect());
    let page_count = pages.len();
    let reviewed_count = pages.iter().filter(|page| s.reviewed_proofs.contains(**page)).count();
    let selected = pages.iter().position(|(path, _)| s.proof_path.as_deref() == Some(path.as_str()));
    let selected_reviewed = selected.and_then(|index| pages.get(index)).is_some_and(|page| s.reviewed_proofs.contains(*page));
    let mut select_page = None;
    let mut confirm_page = false;
    let mut confirm_pair = false;
    let can_review_pair = s.can_review_pair();
    let Some(batch) = &s.snapshot.batch else {
        return;
    };
    ui.separator();
    label(ui, a, "Page proof review", "مراجعة معاينات الصفحات");
    rtl::label(
        ui,
        format!(
            "{} {} / {} · {} {} / {} · {}",
            text(a, "Review group", "مجموعة المراجعة"),
            batch.current_batch,
            batch.total_batches,
            text(a, "Pages", "الصفحات"),
            batch.completed_pages,
            batch.total_pages,
            if a { rtl::ltr(&batch.status) } else { batch.status.clone() }
        ),
    );
    if let Some(current) = batch.batches.iter().find(|report| report.batch == batch.current_batch).filter(|_| !pages.is_empty()) {
        egui::ScrollArea::horizontal().id_salt("ai_review_page_strip").max_height(30.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                for (index, page) in pages.iter().enumerate() {
                    let number = u64::from(current.page_start).saturating_add(index as u64).saturating_add(1);
                    let title =
                        format!("{} {number}{}", text(a, "Review page", "مراجعة الصفحة"), if s.reviewed_proofs.contains(*page) { " ✓" } else { "" });
                    if ui.selectable_label(selected == Some(index), rtl::widget(ui, title)).clicked() {
                        select_page = Some(page.0.clone());
                    }
                }
            });
        });
        let pair = selected
            .and_then(|index| current.page_start.checked_add(u32::try_from(index).ok()?))
            .filter(|_| facing_pages && s.proof_pair_view)
            .and_then(|page| {
                facing_proof_pair(page, s.proof_binding.or(s.settings.right_to_left_binding).unwrap_or_else(|| automatic_proof_binding(&s.prompt, a)))
            });
        let height = (ui.available_height() - 88.0).clamp(300.0, 480.0);
        let action_width = (ui.available_width() * 0.36).clamp(190.0, 255.0);
        let image_width = (ui.available_width() - action_width - ui.spacing().item_spacing.x).max(100.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(egui::vec2(image_width, height), egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.set_width(image_width);
                ui.set_min_height(height);
                egui::ScrollArea::both().id_salt("ai_proof_zoom_canvas").auto_shrink([false, false]).max_width(image_width).max_height(height).show(
                    ui,
                    |ui| {
                        if let Some(pair) = pair {
                            let page_width = if s.proof_full_resolution {
                                s.proof_texture.as_ref().map_or(image_width, |texture| texture.size_vec2().x)
                            } else {
                                ((image_width - ui.spacing().item_spacing.x) * 0.5).max(40.0)
                            };
                            ui.allocate_ui_with_layout(egui::vec2(image_width, height), egui::Layout::left_to_right(egui::Align::Min), |ui| {
                                for page in pair {
                                    ui.push_id(("facing_proof_page", page), |ui| {
                                        ui.allocate_ui_with_layout(
                                            egui::vec2(page_width, height),
                                            egui::Layout::top_down(egui::Align::Center),
                                            |ui| {
                                                ui.set_width(page_width);
                                                let page_number = page.saturating_add(1);
                                                let proof = page
                                                    .checked_sub(current.page_start)
                                                    .and_then(|index| usize::try_from(index).ok())
                                                    .and_then(|index| pages.get(index));
                                                if let Some(proof) = proof {
                                                    let (path, _) = proof;
                                                    let is_selected = s.proof_path.as_deref() == Some(path.as_str());
                                                    let title = format!(
                                                        "{} {}{}{}",
                                                        text(a, "Page", "الصفحة"),
                                                        page_number,
                                                        if is_selected { text(a, " · Selected", " · محددة") } else { "" },
                                                        if s.reviewed_proofs.contains(*proof) {
                                                            text(a, " · Reviewed", " · تمت المراجعة")
                                                        } else {
                                                            ""
                                                        }
                                                    );
                                                    if ui.selectable_label(is_selected, rtl::widget(ui, title)).clicked() {
                                                        select_page = Some(path.clone());
                                                    }
                                                    let (texture, error) = if is_selected {
                                                        (s.proof_texture.as_ref(), s.proof_error.as_str())
                                                    } else if let Some(companion) = &s.companion_proof
                                                        && companion.path.as_str() == path.as_str()
                                                    {
                                                        (companion.texture.as_ref(), companion.error.as_str())
                                                    } else {
                                                        (None, "")
                                                    };
                                                    let alt =
                                                        format!("{} {} {}", text(a, "Page", "الصفحة"), page_number, text(a, "proof", "للمراجعة"));
                                                    let size = if s.proof_full_resolution {
                                                        texture.map_or(egui::vec2(page_width, height), |texture| texture.size_vec2())
                                                    } else {
                                                        egui::vec2(page_width, height - 48.0)
                                                    };
                                                    if proof_image(ui, a, texture, error, &alt, size).is_some_and(|response| response.clicked()) {
                                                        select_page = Some(path.clone());
                                                    }
                                                } else if page < batch.total_pages {
                                                    rtl::label(ui, format!("{} {}", text(a, "Page", "الصفحة"), page_number));
                                                    label(ui, a, "Outside this review group", "خارج مجموعة المراجعة الحالية");
                                                } else {
                                                    label(ui, a, "End of book", "نهاية الكتاب");
                                                }
                                            },
                                        );
                                    });
                                }
                            });
                        } else {
                            let _ = proof_image(
                                ui,
                                a,
                                s.proof_texture.as_ref(),
                                &s.proof_error,
                                text(a, "Selected page proof", "معاينة الصفحة المحددة"),
                                if s.proof_full_resolution {
                                    s.proof_texture.as_ref().map_or(egui::vec2(image_width, height), |texture| texture.size_vec2())
                                } else {
                                    egui::vec2(image_width, height)
                                },
                            );
                        }
                    },
                );
            });
            ui.allocate_ui_with_layout(egui::vec2(action_width, height), egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_width(action_width);
                egui::ScrollArea::vertical().id_salt("ai_review_actions").max_height(height).show(ui, |ui| {
                    ui.set_width(action_width);
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    rtl::label(ui, format!("{} {} / {}", text(a, "Pages reviewed:", "الصفحات المراجعة:"), reviewed_count, page_count));
                    if facing_pages
                        && ui.checkbox(&mut s.proof_pair_view, rtl::widget(ui, text(a, "Show facing pair", "عرض الصفحتين المتقابلتين"))).changed()
                    {
                        s.review_pointer_press = None;
                    }
                    if ui.checkbox(&mut s.proof_full_resolution, rtl::widget(ui, text(a, "100% image preview", "معاينة الصورة بحجم 100%"))).changed()
                    {
                        s.review_pointer_press = None;
                    }
                    if let Some(index) = selected {
                        rtl::label(
                            ui,
                            format!(
                                "{} {} / {} · {} {}",
                                text(a, "Preview", "المعاينة"),
                                index.saturating_add(1),
                                page_count,
                                text(a, "Document page", "صفحة المستند"),
                                current.page_start.saturating_add(u32::try_from(index).unwrap_or(u32::MAX)).saturating_add(1)
                            ),
                        );
                        ui.horizontal(|ui| {
                            if ui.add_enabled(index > 0, egui::Button::new(rtl::widget(ui, text(a, "Previous", "السابقة")))).clicked() {
                                select_page = index.checked_sub(1).and_then(|previous| pages.get(previous)).map(|(path, _)| path.clone());
                            }
                            if ui
                                .add_enabled(index.saturating_add(1) < page_count, egui::Button::new(rtl::widget(ui, text(a, "Next", "التالية"))))
                                .clicked()
                            {
                                select_page = pages.get(index.saturating_add(1)).map(|(path, _)| path.clone());
                            }
                        });
                        egui::ComboBox::from_id_salt("ai_proof_page")
                            .selected_text(rtl::widget(
                                ui,
                                format!(
                                    "{} {}",
                                    text(a, "Page", "الصفحة"),
                                    u64::from(current.page_start).saturating_add(index as u64).saturating_add(1)
                                ),
                            ))
                            .show_ui(ui, |ui| {
                                for (page_index, page) in pages.iter().enumerate() {
                                    let (path, _) = page;
                                    let reviewed = s.reviewed_proofs.contains(*page);
                                    let title = format!(
                                        "{} {}{}",
                                        text(a, "Page", "الصفحة"),
                                        u64::from(current.page_start).saturating_add(page_index as u64).saturating_add(1),
                                        if reviewed { text(a, " · Reviewed", " · تمت المراجعة") } else { "" }
                                    );
                                    if ui.selectable_label(page_index == index, rtl::widget(ui, title)).clicked() {
                                        select_page = Some(path.clone());
                                    }
                                }
                            });
                    }
                    if batch.status == "awaiting_human_proof" {
                        if pair_visible {
                            ui.push_id(("ai_review_pair", s.proof_path.as_deref()), |ui| {
                                let response = ui.add_enabled(
                                    can_review_pair,
                                    egui::Button::new(rtl::widget(
                                        ui,
                                        text(a, "I reviewed both displayed pages; next preview", "راجعت الصفحتين المعروضتين؛ المعاينة التالية"),
                                    ))
                                    .wrap(),
                                );
                                confirm_pair = AiUiState::fresh_review_gesture(
                                    s.review_input_is_human,
                                    s.proof_key.as_ref(),
                                    &mut s.review_pointer_press,
                                    &response,
                                );
                            });
                            if !can_review_pair && !s.proof_full_resolution {
                                label(
                                    ui,
                                    a,
                                    "Enable 100% image preview to confirm both facing pages together.",
                                    "فعّل معاينة الصورة بحجم 100% لتأكيد الصفحتين المتقابلتين معاً.",
                                );
                            }
                        }
                        let review_label = if selected_reviewed {
                            text(a, "Reviewed", "تمت المراجعة")
                        } else {
                            text(a, "I reviewed the displayed page", "أؤكد مراجعة الصفحة المعروضة")
                        };
                        // A held press on one page must never confirm a different preview.
                        ui.push_id(("ai_review_page", s.proof_path.as_deref()), |ui| {
                            let response = ui.add_enabled(can_review && !selected_reviewed, egui::Button::new(rtl::widget(ui, review_label)).wrap());
                            confirm_page = AiUiState::fresh_review_gesture(
                                s.review_input_is_human,
                                s.proof_key.as_ref(),
                                &mut s.review_pointer_press,
                                &response,
                            );
                        });
                        ui.separator();
                        let (en, ar) = if batch.current_batch >= batch.total_batches {
                            ("Approve final proof and finish", "اعتماد المراجعة النهائية وإنهاء المهمة")
                        } else {
                            ("Approve and generate next review group", "اعتماد وإنشاء مجموعة المراجعة التالية")
                        };
                        let response = ui.add_enabled(can_approve, egui::Button::new(rtl::widget(ui, text(a, en, ar))).wrap());
                        if AiUiState::fresh_review_gesture(s.review_input_is_human, s.proof_key.as_ref(), &mut s.review_pointer_press, &response) {
                            s.batch_approval_gesture = s.proof_key.clone().map(|key| (key, ui.ctx().clone(), ui.ctx().cumulative_frame_nr()));
                            requests.push(AiRequest::ApproveBatch { job_id: batch.job_id.clone(), checkpoint: batch.checkpoint });
                        }
                        if page_count == 0 {
                            label(ui, a, "Verified previews for every group page are required.", "يلزم توفر صور موثقة لجميع صفحات المجموعة.");
                        } else if !s.proof_document_matches {
                            label(
                                ui,
                                a,
                                "Return to the proof's document revision. Changes require a new proof.",
                                "ارجع إلى إصدار المستند المطابق للبروفة. التعديلات تتطلب بروفة جديدة.",
                            );
                            let same_book_edited = s.proof_document_version.map(|(uid, _)| uid).is_some()
                                && s.proof_document_version.map(|(uid, _)| uid) == batch.document_uid;
                            if same_book_edited
                                && ui
                                    .add_enabled(
                                        !s.snapshot.busy,
                                        egui::Button::new(rtl::widget(
                                            ui,
                                            text(a, "Refresh proofs for edited document", "تحديث البروفات للمستند المعدل"),
                                        ))
                                        .wrap(),
                                    )
                                    .clicked()
                            {
                                requests.push(AiRequest::RefreshBatchProofs { job_id: batch.job_id.clone() });
                            }
                        } else if reviewed_count < page_count {
                            label(ui, a, "Confirm each displayed page to enable approval.", "أكد مراجعة كل صفحة معروضة لتفعيل الاعتماد.");
                        }
                    }
                });
            });
        });
    } else if matches!(batch.status.as_str(), "failed" | "interrupted" | "cancelled") {
        label(
            ui,
            a,
            "This group has no complete published page proofs. Resolve the reported error and retry the unfinished pages; there is nothing new to approve.",
            "لا توجد بروفات منشورة مكتملة لهذه المجموعة. عالج الخطأ الظاهر ثم أعد محاولة الصفحات غير المكتملة؛ لا يوجد ناتج جديد لاعتماده.",
        );
    } else {
        label(ui, a, "Current group usage and proof have not yet been reported.", "لم تُبلغ بيانات استخدام المجموعة الحالية ومعاينتها بعد.");
    }
    egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Batch details", "تفاصيل الدفعات"))).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("ai_batch_details").max_height(160.0).show(ui, |ui| {
            rtl::label(
                ui,
                format!(
                    "{} / checkpoint {} / document {:?} / revision {:?}",
                    batch.job_id, batch.checkpoint, batch.document_uid, batch.document_revision
                ),
            );
            if let Some(style) = s
                .snapshot
                .reports
                .iter()
                .rev()
                .find(|report| report.id.starts_with(&format!("{}-batch-", batch.job_id)))
                .and_then(|report| report.design_style.as_ref())
            {
                rtl::label(ui, format!("{}: {} · {}", text(a, "Pinned design style", "نمط التصميم المثبّت"), style.name, style.revision));
            }
            label(ui, a, "Cumulative task usage", "الاستخدام التراكمي للمهمة");
            usage_line(ui, a, &batch.cumulative_usage, batch.elapsed_ms);
            for report in &batch.batches {
                rtl::label(
                    ui,
                    format!(
                        "{} {} · {} {}–{} · {}",
                        text(a, "Batch", "الدفعة"),
                        report.batch,
                        text(a, "Pages", "الصفحات"),
                        report.page_start.saturating_add(1),
                        report.page_start.saturating_add(report.page_count),
                        report.status
                    ),
                );
                usage_line(ui, a, &report.usage, report.elapsed_ms);
                for path in &report.proof_paths {
                    ui.add(egui::Label::new(rtl::widget(ui, path)).wrap());
                }
            }
        });
    });
    if confirm_page {
        s.review_selected_proof();
        ui.ctx().request_repaint();
    }
    if confirm_pair {
        s.review_displayed_pair();
        ui.ctx().request_repaint();
    }
    if let Some(path) = select_page
        && s.select_proof(&path)
    {
        ui.ctx().request_repaint();
    }
}

fn image_inspection_setting(ui: &mut egui::Ui, s: &mut AiUiState, a: bool) {
    let supported_model = s.settings.text_model.provider == ProviderKind::Gemini
        && (s.settings.text_model.model.trim().is_empty()
            || supports_gemini_image_review(s.settings.text_model.provider, &s.settings.text_model.model));
    ui.add_enabled(
        supported_model,
        egui::Checkbox::new(
            &mut s.settings.verify_generated_images,
            rtl::widget(
                ui,
                text(
                    a,
                    "Inspect generated images with Gemini (extra billed vision requests)",
                    "فحص الصور المولدة مع Gemini (طلبات رؤية إضافية مدفوعة)",
                ),
            ),
        ),
    );
    label(
        ui,
        a,
        "Optional advisory inspection; human proof review is still required. Requires a supported Gemini text model, validated before use.",
        "فحص استشاري اختياري؛ تبقى مراجعة البروفة البشرية مطلوبة. يتطلب نموذج نص Gemini مدعوماً، ويُتحقق من دعمه قبل الاستخدام.",
    );
    if !supported_model && s.settings.verify_generated_images && button(ui, a, "Turn off image inspection", "إيقاف فحص الصور") {
        s.settings.verify_generated_images = false;
    }
}

fn budgets(ui: &mut egui::Ui, s: &mut AiUiState, a: bool) {
    ui.checkbox(
        &mut s.settings.allow_paid_repairs,
        rtl::widget(ui, text(a, "Allow extra automatic planning requests (may be billed)", "السماح بطلبات تخطيط إضافية تلقائية قد تكون مدفوعة")),
    );
    let repairs = s.settings.allow_paid_repairs;
    let b = &mut s.settings.budget;
    egui::Grid::new("ai_limits").num_columns(2).show(ui, |ui| {
        label(ui, a, "Total tokens", "مجموع الرموز");
        ui.add(egui::DragValue::new(&mut b.token_limit).range(256..=MAX_TOKEN_LIMIT));
        ui.end_row();
        label(ui, a, "Output tokens per request", "رموز الإخراج لكل طلب");
        ui.add(egui::DragValue::new(&mut b.max_output_tokens).range(1..=65_536));
        ui.end_row();
        label(ui, a, "Maximum requests", "أقصى عدد للطلبات");
        ui.add(egui::DragValue::new(&mut b.max_requests).range(1..=MAX_REQUEST_ALLOWANCE));
        ui.end_row();
        label(ui, a, "Maximum extra planning requests", "أقصى طلبات تخطيط إضافية");
        ui.add_enabled(repairs, egui::DragValue::new(&mut b.repair_attempts).range(0..=3));
        ui.end_row();
    });
    let mut cap = b.max_cost_usd.is_some();
    if ui.checkbox(&mut cap, rtl::widget(ui, text(a, "Estimated cost limit (USD)", "حد التكلفة التقديرية بالدولار"))).changed()
    {
        b.max_cost_usd = cap.then_some(1.0);
    }
    if let Some(cost) = &mut b.max_cost_usd {
        ui.add(egui::DragValue::new(cost).range(0.001..=10_000.0).speed(0.01));
        let rates = b.rates.get_or_insert(TokenRates {
            input_per_million: 0.0,
            cached_input_per_million: 0.0,
            output_per_million: 0.0,
            cache_write_per_million: None,
            image_per_request: None,
            image_reserve_usd: None,
        });
        label(
            ui,
            a,
            "Enter your actual rates per million tokens. Estimates are not invoices.",
            "أدخل الأسعار الفعلية لكل مليون رمز. التقديرات ليست فواتير.",
        );
        for (en, ar, rate) in [
            ("Input", "الإدخال", &mut rates.input_per_million),
            ("Cached input", "الإدخال المخزن", &mut rates.cached_input_per_million),
            ("Output", "الإخراج", &mut rates.output_per_million),
        ] {
            ui.horizontal(|ui| {
                label(ui, a, en, ar);
                ui.add(egui::DragValue::new(rate).range(0.0..=100_000.0).speed(0.01));
            });
        }
        optional_rate(ui, a, "Cache-write / million", "كتابة التخزين / مليون", &mut rates.cache_write_per_million);
        if s.settings.output_mode == OutputMode::NativeWithImages {
            ui.separator();
            label(ui, a, "Separate image provider rates", "أسعار مزود الصور المستقلة");
            let image = s.settings.image_rates.get_or_insert(TokenRates {
                input_per_million: 0.0,
                cached_input_per_million: 0.0,
                output_per_million: 0.0,
                cache_write_per_million: None,
                image_per_request: None,
                image_reserve_usd: None,
            });
            for (en, ar, rate) in [
                ("Image input / million", "إدخال الصور / مليون", &mut image.input_per_million),
                ("Image cached input / million", "إدخال الصور المخزن / مليون", &mut image.cached_input_per_million),
                ("Image output / million", "إخراج الصور / مليون", &mut image.output_per_million),
            ] {
                ui.horizontal(|ui| {
                    label(ui, a, en, ar);
                    ui.add(egui::DragValue::new(rate).range(0.0..=100_000.0).speed(0.01));
                });
            }
            optional_rate(ui, a, "Image cache-write / million", "كتابة تخزين الصور / مليون", &mut image.cache_write_per_million);
            optional_rate(ui, a, "Fixed price / image request", "سعر ثابت / طلب صورة", &mut image.image_per_request);
            if image.image_per_request.is_none() {
                optional_rate(ui, a, "Image request budget reserve (USD)", "احتياط كلفة طلب الصورة بالدولار", &mut image.image_reserve_usd);
                label(
                    ui,
                    a,
                    "Token pricing: reserve before sending, then calculate from reported usage.",
                    "تسعير بالتوكنز: يحجز الاحتياط قبل الإرسال ثم يحتسب من الاستهلاك المبلّغ.",
                );
            } else {
                label(
                    ui,
                    a,
                    "Image cost uses the fixed request price; reported tokens remain visible.",
                    "تحتسب كلفة الصور بالسعر الثابت للطلب، مع عرض التوكنز المبلّغة.",
                );
            }
        }
        ui.checkbox(
            &mut s.rates_confirmed,
            rtl::widget(ui, text(a, "I entered the rates to use for this estimate", "أؤكد إدخال الأسعار المستخدمة في هذا التقدير")),
        );
    }
}
fn optional_rate(ui: &mut egui::Ui, a: bool, en: &str, ar: &str, rate: &mut Option<f64>) {
    ui.horizontal(|ui| {
        let mut enabled = rate.is_some();
        if ui.checkbox(&mut enabled, rtl::widget(ui, text(a, en, ar))).changed() {
            *rate = enabled.then_some(0.0);
        }
        if let Some(value) = rate {
            ui.add(egui::DragValue::new(value).range(0.0..=100_000.0).speed(0.01));
        }
    });
}
fn export_print(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, enabled: bool, requests: &mut Vec<AiRequest>) {
    field(
        ui,
        a,
        "Export file path (the selected format sets its extension)",
        "مسار ملف التصدير (يُضبط الامتداد حسب الصيغة المختارة)",
        &mut s.export_path,
    );
    ui.horizontal(|ui| {
        for (pdf, en, ar) in [(false, "Export native", "تصدير أصلي"), (true, "Export PDF", "تصدير PDF")] {
            if ui.add_enabled(enabled && !s.export_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, en, ar)))).clicked() {
                s.export_path =
                    std::path::Path::new(&s.export_path).with_extension(if pdf { "pdf" } else { "designcraft" }).to_string_lossy().into_owned();
                requests.push(AiRequest::ExportDocument { path: s.export_path.clone(), pdf });
            }
        }
        if ui
            .add_enabled(enabled && !s.export_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Export IDML", "تصدير IDML"))))
            .clicked()
        {
            s.export_path = std::path::Path::new(&s.export_path).with_extension("idml").to_string_lossy().into_owned();
            requests.push(AiRequest::ExportIdml { path: s.export_path.clone() });
        }
        if ui
            .add_enabled(enabled && !s.export_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Export INDD", "تصدير INDD"))))
            .clicked()
        {
            s.export_path = std::path::Path::new(&s.export_path).with_extension("indd").to_string_lossy().into_owned();
            requests.push(AiRequest::ExportIndd { path: s.export_path.clone() });
        }
    });
    label(
        ui,
        a,
        "INDD requires installed InDesign. Linked raster assets are currently unsupported for this conversion.",
        "يتطلب INDD تثبيت InDesign. لا تدعم عملية التحويل هذه الصور النقطية المرتبطة حالياً.",
    );
    egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Print — actual size (100%)", "طباعة بالحجم الفعلي (١٠٠٪)"))).show(ui, |ui| {
        field(ui, a, "Printer (empty = system default)", "الطابعة (فارغ = الافتراضية)", &mut s.printer);
        ui.checkbox(&mut s.grayscale, rtl::widget(ui, text(a, "Grayscale", "تدرج رمادي")));
        let duplex_options = [
            ("off", "One side", "وجه واحد"),
            ("longEdge", "Duplex long edge", "وجهان: الحافة الطويلة"),
            ("shortEdge", "Duplex short edge", "وجهان: الحافة القصيرة"),
        ];
        let duplex_selected =
            duplex_options.iter().find(|(v, _, _)| *v == s.duplex).map(|(_, en, ar)| text(a, en, ar)).unwrap_or(s.duplex.as_str());
        egui::ComboBox::from_id_salt("ai_duplex").selected_text(rtl::widget(ui, duplex_selected)).show_ui(ui, |ui| {
            for (v, en, ar) in duplex_options {
                ui.selectable_value(&mut s.duplex, v.into(), rtl::widget(ui, text(a, en, ar)));
            }
        });
        if ui.add_enabled(enabled, egui::Button::new(rtl::widget(ui, text(a, "Prepare frozen print proof", "إعداد بروفة الطباعة")))).clicked() {
            requests.push(AiRequest::PreparePrint { printer: (!s.printer.trim().is_empty()).then(|| s.printer.clone()), grayscale: s.grayscale, duplex: s.duplex.clone() });
        }
        if let Some(proof) = s.snapshot.print_proof.clone() {
            ui.separator();
            rtl::label(ui, format!("{} {} · {} {}", text(a, "Pages", "الصفحات"), proof.pages, text(a, "PDF bytes", "حجم PDF بالبايت"), proof.bytes));
            ui.add(egui::Label::new(format!("SHA-256: {}", proof.sha256)).wrap());
            if let Some(path) = &proof.pdf_path { ui.add(egui::Label::new(path).wrap()); }
            if button(ui, a, "Open complete PDF proof", "فتح PDF البروفة كاملاً") { requests.push(AiRequest::OpenPrintProof { token: proof.token.clone() }); }
            for (key, en, ar) in [("fonts", "Fonts", "الخطوط"), ("color", "Color settings", "إعدادات الألوان"), ("job", "Frozen printer queue and job settings", "الطابعة وإعدادات مهمة البروفة")] {
                egui::CollapsingHeader::new(rtl::widget(ui, text(a, en, ar))).default_open(key == "job").show(ui, |ui| {
                    if let Some(value) = proof.manifest.get(key) { ui.add(egui::Label::new(serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())).wrap()); }
                });
            }
            label(ui, a, "Changing options requires a new proof. Submission uses this frozen PDF and job; printer/RIP settings can affect physical scaling and color.", "يتطلب تغيير الخيارات إعداد مراجعة جديدة. تُرسل نسخة PDF والمهمة المجمدتان؛ وقد تؤثر إعدادات الطابعة أو RIP في قياس الورق واللون.");
            field(ui, a, "Save exact proof PDF path", "مسار حفظ PDF المطابق للبروفة", &mut s.print_save_path);
            if ui.add_enabled(!s.print_save_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Save frozen PDF", "حفظ PDF البروفة نفسه")))).clicked() {
                s.print_save_path = std::path::Path::new(&s.print_save_path).with_extension("pdf").to_string_lossy().into_owned();
                requests.push(AiRequest::SavePreparedPrint { token: proof.token.clone(), path: s.print_save_path.clone() });
            }
            ui.checkbox(&mut s.print_reviewed, rtl::widget(ui, text(a, "I reviewed this PDF, fonts, colors and printer queue", "راجعت ملف PDF والخطوط والألوان والطابعة لهذه البروفة")));
            if ui.add_enabled(enabled && s.print_reviewed, egui::Button::new(rtl::widget(ui, text(a, "Submit this frozen job to printer", "إرسال البروفة نفسها إلى الطابعة")))).clicked() {
                requests.push(AiRequest::SubmitPreparedPrint { token: proof.token, request_id: format!("ui-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)) });
                s.print_reviewed = false;
            }
        }

    });
}

fn skills(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, available: bool, document: bool, requests: &mut Vec<AiRequest>) {
    ui.add_enabled_ui(available && !s.task_active(), |ui| {
        egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Design style library", "مكتبة أنماط التصميم"))).default_open(true).show(ui, |ui| {
            label(
                ui,
                a,
                "Reusable rules, native page families and recorded corrections. Technical checks do not replace educator or visual review.",
                "قواعد وقوالب أصلية وتصحيحات محفوظة. الفحص التقني لا يغني عن المراجعة التربوية والبصرية.",
            );
            let mut style_cards: Vec<&SkillCard> = Vec::new();
            for card in &s.snapshot.skills {
                if card.design_profile.is_some() {
                    if let Some(existing) = style_cards.iter_mut().find(|existing| existing.id == card.id) {
                        if card.active || !existing.active {
                            *existing = card;
                        }
                    } else {
                        style_cards.push(card);
                    }
                }
            }
            for card in style_cards {
                if let Some(profile) = &card.design_profile {
                    ui.horizontal_wrapped(|ui| {
                        rtl::label(ui, &profile.name);
                        rtl::label(
                            ui,
                            format!(
                                "{}–{} · {} × {} mm · {} mm · {} {}",
                                profile.age_range[0],
                                profile.age_range[1],
                                profile.page_mm[0],
                                profile.page_mm[1],
                                profile.margin_mm,
                                profile.page_families.len(),
                                text(a, "page families", "قوالب صفحات")
                            ),
                        );
                        if button(ui, a, "Inspect style", "فحص النمط") {
                            requests.push(AiRequest::InspectSkill { id: card.id.clone(), revision: card.revision.clone() });
                        }
                        if ui
                            .add_enabled(
                                card.enabled && card.compatible && (s.automatic_skills || card.active || card.trusted_source),
                                egui::Button::new(rtl::widget(ui, text(a, "Use style", "استخدام النمط"))),
                            )
                            .clicked()
                        {
                            s.settings.design_style = Some(card.id.clone());
                            s.tab = 0;
                        }
                    });
                }
            }
        });
        field(ui, a, "Import skill folder or ZIP path", "مسار مجلد المهارة أو ملف ZIP", &mut s.import_path);
        if ui.add_enabled(!s.import_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Import draft", "استيراد مسودة")))).clicked()
        {
            requests.push(AiRequest::ImportSkill { path: s.import_path.clone() });
        }
        ui.separator();
        let name_width = (ui.available_width() - 449.0).max(48.0);
        let version_width = 90.0;
        let status_width = 95.0;
        ui.horizontal(|ui| {
            for (en, ar, width) in [
                ("Name", "الاسم", name_width),
                ("Version", "الإصدار", version_width),
                ("Status", "الحالة", status_width),
                ("Actions", "الإجراءات", 240.0),
            ] {
                ui.add_sized([width, 20.0], egui::Label::new(rtl::widget(ui, text(a, en, ar))).truncate());
            }
        });
        for c in &s.snapshot.skills {
            ui.push_id((&c.id, &c.revision), |ui| {
                ui.horizontal(|ui| {
                    ui.add_sized([name_width, 20.0], egui::Label::new(rtl::widget(ui, &c.name)).truncate()).on_hover_text(&c.name);
                    let revision: String = c.revision.chars().take(12).collect();
                    ui.add_sized([version_width, 20.0], egui::Label::new(revision).truncate()).on_hover_text(&c.revision);
                    let mut status = if c.automatically_validated {
                        text(a, "Auto checked", "فُحصت تلقائياً")
                    } else {
                        text(a, if c.approved { "Approved" } else { "Draft" }, if c.approved { "معتمدة" } else { "مسودة" })
                    }
                    .to_owned();
                    if c.active {
                        status.push_str(text(a, " / Active", " / نشطة"));
                    }
                    if !c.compatible {
                        status.push_str(text(a, " / Incompatible", " / غير متوافقة"));
                    }
                    ui.add_sized([status_width, 20.0], egui::Label::new(rtl::widget(ui, &status)).truncate()).on_hover_text(&status);
                    if button(ui, a, "Inspect", "مراجعة") {
                        requests.push(AiRequest::InspectSkill { id: c.id.clone(), revision: c.revision.clone() });
                    }
                    let mut enabled = c.enabled && c.compatible;
                    if ui
                        .add_enabled(c.compatible, egui::Checkbox::new(&mut enabled, rtl::widget(ui, text(a, "Use skill", "استخدام المهارة"))))
                        .changed()
                    {
                        requests.push(AiRequest::SetSkillEnabled { id: c.id.clone(), enabled });
                    }
                    if ui
                        .add_enabled(c.approved && !c.active, egui::Button::new(rtl::widget(ui, text(a, "Restore revision", "استعادة الإصدار"))))
                        .clicked()
                    {
                        requests.push(AiRequest::RollbackSkill { id: c.id.clone(), revision: c.revision.clone() });
                    }
                });
                ui.scope(|ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
                    rtl::label(ui, &c.description);
                });
                ui.separator();
            });
        }
        if let Some(r) = s.snapshot.review.clone() {
            ui.separator();
            rtl::label(ui, format!("{} / {}", r.id, r.revision));
            if let Some(profile) =
                s.snapshot.skills.iter().find(|card| card.id == r.id && card.revision == r.revision).and_then(|card| card.design_profile.as_ref())
            {
                egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Style rules and correction examples", "قواعد النمط وأمثلة التصحيح")))
                    .default_open(true)
                    .show(ui, |ui| {
                        for (en, ar, rules) in [
                            ("Typography", "الخطوط", &profile.typography),
                            ("Layout", "التوزيع", &profile.layout),
                            ("Pedagogy", "التدرج التعليمي", &profile.pedagogy),
                            ("Illustrations", "الرسوم", &profile.illustration),
                        ] {
                            label(ui, a, en, ar);
                            for rule in rules {
                                ui.add(egui::Label::new(rtl::widget(ui, rule)).wrap());
                            }
                        }
                        for family in &profile.page_families {
                            ui.horizontal_wrapped(|ui| {
                                rtl::label(ui, format!("{}: {}", family.id, family.purpose));
                                if button(ui, a, "Preview page family", "معاينة قالب الصفحة") {
                                    requests.push(AiRequest::RunDesignTemplate {
                                        id: r.id.clone(),
                                        revision: r.revision.clone(),
                                        template: family.template.clone(),
                                    });
                                }
                            });
                        }
                        for correction in &profile.corrections {
                            ui.add(
                                egui::Label::new(rtl::widget(ui, format!("{} → {} ({})", correction.before, correction.after, correction.reason)))
                                    .wrap(),
                            );
                        }
                    });
                egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Save a design correction", "حفظ تصحيح للنمط"))).show(ui, |ui| {
                    field(ui, a, "Before", "قبل التصحيح", &mut s.correction_before);
                    field(ui, a, "After", "المطلوب بعد التصحيح", &mut s.correction_after);
                    field(ui, a, "Reason", "سبب التصحيح", &mut s.correction_reason);
                    let ready = [&s.correction_before, &s.correction_after, &s.correction_reason]
                        .iter()
                        .all(|value| !value.trim().is_empty() && value.len() <= 512);
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(rtl::widget(ui, text(a, "Save correction as a new revision", "حفظ التصحيح بإصدار جديد"))),
                        )
                        .clicked()
                    {
                        requests.push(AiRequest::AddDesignCorrection {
                            id: r.id.clone(),
                            revision: r.revision.clone(),
                            correction: designcraft_ai_types::DesignCorrection {
                                before: s.correction_before.clone(),
                                after: s.correction_after.clone(),
                                reason: s.correction_reason.clone(),
                            },
                        });
                    }
                    label(
                        ui,
                        a,
                        "Explicit reusable guidance; this does not train model weights or approve a page batch.",
                        "توجيه محفوظ للاستعمال لاحقاً؛ لا يدرّب أوزان النموذج ولا يعتمد دفعة صفحات.",
                    );
                });
            }
            label(ui, a, "SKILL.md — edits create a new draft", "SKILL.md — التعديل ينشئ مسودة جديدة");
            let mut skill_md_layouter = bidi_layouter(true);
            rtl::show_text_edit(
                ui,
                egui::TextEdit::multiline(&mut s.skill_md).desired_rows(8).desired_width(f32::INFINITY).layouter(&mut skill_md_layouter),
            );
            label(ui, a, "Manifest JSON", "ملف الوصف JSON");
            ui.add(egui::TextEdit::multiline(&mut s.manifest_json).code_editor().desired_rows(5).desired_width(f32::INFINITY));
            let clean = s.skill_md == r.skill_md && s.manifest_json == r.manifest_json;
            ui.horizontal_wrapped(|ui| {
                if button(ui, a, "Save as draft", "حفظ كمسودة") {
                    requests.push(AiRequest::SaveSkillDraft {
                        id: r.id.clone(),
                        skill_md: s.skill_md.clone(),
                        manifest_json: s.manifest_json.clone(),
                    });
                }
                if ui.add_enabled(clean, egui::Button::new(rtl::widget(ui, text(a, "Run sample", "تشغيل العينة")))).clicked() {
                    requests.push(AiRequest::RunSkillSample { id: r.id.clone(), revision: r.revision.clone() });
                }
                if clean && r.sample_valid && button(ui, a, "Approve this revision", "اعتماد هذا الإصدار") {
                    requests.push(AiRequest::ApproveSkill { id: r.id.clone(), revision: r.revision.clone() });
                }
            });
            label(
                ui,
                a,
                if r.sample_valid {
                    "Sample passed; inspect it before explicit approval."
                } else {
                    "Approval requires a valid sample of this saved revision."
                },
                if r.sample_valid {
                    "نجحت العينة؛ افحصها قبل الاعتماد الصريح."
                } else {
                    "يتطلب الاعتماد عينة ناجحة لهذا الإصدار المحفوظ."
                },
            );
            if let Some(path) = &r.sample_path {
                rtl::label(ui, path);
            }
            if let Some(texture) = &s.preview {
                ui.add(egui::Image::new(texture).max_size(egui::vec2(600.0, 420.0)));
            }
            if !s.preview_error.is_empty() {
                rtl::label(ui, &s.preview_error);
            }
            egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Files", "الملفات"))).show(ui, |ui| {
                for f in &r.files {
                    rtl::label(ui, f);
                }
            });
            field(ui, a, "Export skill ZIP path", "مسار تصدير المهارة ZIP", &mut s.skill_export_path);
            if ui
                .add_enabled(!s.skill_export_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Export skill", "تصدير المهارة"))))
                .clicked()
            {
                requests.push(AiRequest::ExportSkill { id: r.id, revision: r.revision, path: s.skill_export_path.clone() });
            }
        }
        ui.separator();
        label(ui, a, "Generate a reviewable skill draft", "إنشاء مسودة مهارة للمراجعة");
        ui.checkbox(
            &mut s.build_design_style,
            rtl::widget(ui, text(a, "Build a design style with multiple native page templates", "بناء نمط تصميم بقواعد وقوالب صفحات متعددة")),
        );
        let skill_rtl = if s.skill_prompt.trim().is_empty() { a } else { rtl::has_rtl(&s.skill_prompt) };
        let mut skill_layouter = bidi_layouter(true);
        rtl::show_text_edit(
            ui,
            egui::TextEdit::multiline(&mut s.skill_prompt)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .horizontal_align(if skill_rtl { egui::Align::Max } else { egui::Align::Min })
                .layouter(&mut skill_layouter),
        );
        label(ui, a, "Reference paths (one per line)", "مسارات المراجع (مسار في كل سطر)");
        let mut references_layouter = bidi_layouter(true);
        rtl::show_text_edit(
            ui,
            egui::TextEdit::multiline(&mut s.references).desired_rows(3).desired_width(f32::INFINITY).layouter(&mut references_layouter),
        );
        ui.horizontal_wrapped(|ui| {
            let ready = !s.skill_prompt.trim().is_empty()
                && !s.settings.text_model.model.trim().is_empty()
                && !is_non_text_model(&s.settings.text_model.model)
                && s.workflow_ready()
                && (s.settings.budget.max_cost_usd.is_none() || s.rates_confirmed);
            if ui.add_enabled(ready, egui::Button::new(rtl::widget(ui, text(a, "Generate draft", "إنشاء المسودة")))).clicked() {
                requests.push(AiRequest::GenerateSkill {
                    design_style: s.build_design_style,
                    prompt: s.skill_prompt.clone(),
                    reference_paths: s.references.lines().map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned).collect(),
                    settings: s.settings.clone(),
                });
            }
            if ui
                .add_enabled(
                    ready && document,
                    egui::Button::new(rtl::widget(ui, text(a, "Learn from current corrected document", "إنشاء مهارة من المستند المصحح الحالي"))),
                )
                .clicked()
            {
                requests.push(AiRequest::GenerateSkillFromDocument {
                    prompt: s.skill_prompt.clone(),
                    settings: s.settings.clone(),
                    design_style: s.build_design_style,
                });
            }
        });
    });
}

fn usage(ui: &mut egui::Ui, s: &AiUiState, a: bool) {
    label(
        ui,
        a,
        "Provider-reported usage and explicit-rate estimates for this session. Estimates may differ from billing.",
        "الاستخدام المبلغ من المزود وتقدير الأسعار المدخلة لهذه الجلسة. قد يختلف التقدير عن الفاتورة.",
    );
    for r in s.snapshot.reports.iter().rev() {
        let header = if r.title.chars().any(designcraft_fonts::is_rtl) {
            format!("{} · {} · {}", r.title, rtl::ltr(&r.status), rtl::ltr(&r.id))
        } else {
            format!("{} · {} · {}", r.title, r.status, r.id)
        };
        egui::CollapsingHeader::new(rtl::widget(ui, header)).default_open(true).show(ui, |ui| {
            rtl::label(ui, format!("{} / {} · {} ms · {} actions", provider_name(r.provider), r.model, r.elapsed_ms, r.actions));
            if let Some(style) = &r.design_style {
                rtl::label(
                    ui,
                    format!("{}: {} · {}", text(a, "Design style", "نمط التصميم"), style.name, style.revision.chars().take(12).collect::<String>()),
                );
            }
            let u = &r.usage;
            rtl::label(
                ui,
                format!(
                    "Requests: {} | Input: {} | Output: {} | Cached: {} | Reasoning: {} | Estimated: {}",
                    u.requests, u.input_tokens, u.output_tokens, u.cached_input_tokens, u.reasoning_tokens, u.estimated_tokens
                ),
            );
            if let Some(cost) = u.estimated_cost_usd {
                rtl::label(ui, format!("Estimated USD: {cost:.6}"));
            }
            if u.has_unreported_usage {
                label(ui, a, "Some provider usage is unreported.", "بعض بيانات الاستخدام لم يبلغها المزود.");
            }
            rtl::label(ui, &r.message);
            for (id, rev) in &r.skills {
                rtl::label(ui, format!("{id} / {rev}"));
            }
            for path in &r.artifacts {
                rtl::label(ui, path);
            }
            egui::CollapsingHeader::new(rtl::widget(ui, text(a, "Details", "التفاصيل"))).show(ui, |ui| {
                for issue in &r.issues {
                    rtl::label(ui, issue.to_string());
                }
            });
        });
    }
}

fn preview_texture(read: &crate::ReadFn, path: &str, ctx: &egui::Context, name: &str) -> Result<egui::TextureHandle, String> {
    let bytes = read(path)?;
    if bytes.len() > 24 * 1024 * 1024 {
        return Err("Preview exceeds 24 MiB.".into());
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?.thumbnail(1200, 1600).to_rgba8();
    Ok(crate::widgets::load_texture(
        ctx,
        name,
        egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()),
        egui::TextureOptions::LINEAR,
    ))
}

fn load_preview(app: &mut DesignApp, ctx: &egui::Context) {
    let s = &mut app.ui.ai;
    let Some(path) = s.snapshot.review.as_ref().and_then(|r| r.sample_path.clone()) else {
        return;
    };
    if s.preview_path.as_ref() == Some(&path) {
        return;
    }
    s.preview_path = Some(path.clone());
    s.preview_error.clear();
    let Some(read) = &app.services.read else {
        s.preview_error = "Preview reader unavailable.".into();
        return;
    };
    match preview_texture(read, &path, ctx, "ai_skill_sample") {
        Ok(texture) => s.preview = Some(texture),
        Err(e) => s.preview_error = e,
    }
    ctx.request_repaint();
}

fn load_proof_preview(app: &mut DesignApp, ctx: &egui::Context) {
    let s = &mut app.ui.ai;
    let Some(path) = s.proof_path.clone() else {
        return;
    };
    if s.proof_loaded.as_ref() == Some(&path) {
        return;
    }
    s.proof_loaded = Some(path.clone());
    s.proof_error.clear();
    s.proof_texture = None;
    let Some(read) = &app.services.read else {
        s.proof_error = "Proof reader unavailable.".into();
        s.reviewed_proofs.retain(|(page, _)| page != &path);
        return;
    };
    match preview_texture(read, &path, ctx, "ai_batch_proof") {
        Ok(texture) => s.proof_texture = Some(texture),
        Err(e) => {
            s.proof_error = e;
            s.reviewed_proofs.retain(|(page, _)| page != &path);
        }
    }
    ctx.request_repaint();
}

fn load_companion_proof(app: &mut DesignApp, ctx: &egui::Context) {
    let path = app.ui.ai.companion_proof_path().map(str::to_owned);
    if app.ui.ai.companion_proof.as_ref().map(|proof| &proof.path) == path.as_ref() {
        return;
    }
    app.ui.ai.companion_proof = None;
    let Some(path) = path else { return };
    let loaded = match &app.services.read {
        Some(read) => preview_texture(read, &path, ctx, "ai_companion_proof"),
        None => Err("Proof reader unavailable.".into()),
    };
    let (texture, error) = match loaded {
        Ok(texture) => (Some(texture), String::new()),
        Err(error) => {
            app.ui.ai.reviewed_proofs.retain(|(page, _)| page != &path);
            (None, error)
        }
    };
    app.ui.ai.companion_proof = Some(CompanionProof { path, texture, error });
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Services;
    use designcraft_engine::Session;
    use egui_kittest::kittest::{NodeT, Queryable};
    use serde_json::json;

    #[test]
    fn ai23_remaining_book_allowance_is_explicit_and_does_not_change_review_size() {
        let mut state = AiUiState { prompt: "Create a science book".into(), pages: 48, ..Default::default() };
        state.settings.text_model.model = "fixture".into();
        state.settings.pages_per_batch = 2;
        state.settings.output_mode = OutputMode::NativeWithImages;
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 1600.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 4);
        panel.get_by_label("Set request allowance for remaining book").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 96);
        assert_eq!(panel.state().settings.pages_per_batch, 2);
        assert_eq!(panel.state().pages, 48);
    }

    #[test]
    fn ai23_curriculum_preview_uses_the_injected_host_without_starting_production() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = calls.clone();
        let mut app = DesignApp::new(
            Session::new(),
            Services {
                ai: Some(Box::new(move |_, _| {
                    seen.set(seen.get() + 1);
                    AiSnapshot { available: true, message: "local preview".into(), ..Default::default() }
                })),
                ..Default::default()
            },
        );
        let request = TaskRequest {
            prompt: "Science book".into(),
            settings: AiSettings::default(),
            target: TaskTarget::NewA4 { pages: 48 },
            skill_ids: vec![],
            auto_select_skills: true,
            export_directory: None,
        };
        let result = request_command(&mut app, &json!({"method":"preview_production","request":request}));
        assert!(result.is_ok(), "Local curriculum preview should be available: {result:?}");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn ai23_proof_page_strip_navigates_without_reviewing_or_approving() {
        let fixture = ProofFixture::new();
        let calls = fixture.calls.clone();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        panel.get_by_label("Review page 2").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-2.png"));
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        assert_eq!(calls.get(), 0);
        assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
    }

    #[test]
    fn ai23_spread_confirmation_requires_both_current_previews_and_separate_approval() {
        let mut fixture = ProofFixture::new();
        let batch = fixture.checkpoint.batch.as_mut().unwrap();
        batch.total_pages = 6;
        batch.pages_per_batch = 3;
        batch.completed_pages = 3;
        let report = &mut batch.batches[0];
        report.page_count = 3;
        report.proof_paths.push("page-3.png".into());
        report.proof_sha256.push("4".repeat(64));
        fixture.app.ui.ai.accept(fixture.checkpoint.clone());
        fixture.app.ui.ai.settings.facing_pages = true;
        let calls = fixture.calls.clone();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        panel.get_by_label("Next").click();
        panel.run_steps(3);
        assert!(panel.get_by_label("I reviewed both displayed pages; next preview").accesskit_node().is_disabled());
        panel.get_by_label("100% image preview").click();
        panel.run_steps(3);
        panel.get_by_label("I reviewed both displayed pages; next preview").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
        assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-1.png"));
        assert_eq!(calls.get(), 0, "spread review never launches the next group");
        assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
    }

    #[test]
    fn ai20_ui_completed_discovery_stays_selectable_for_its_provider_and_endpoint() {
        let mut state = AiUiState { initialized: true, ..Default::default() };
        state.settings.text_model =
            ModelSelection { provider: ProviderKind::Gemini, model: "original".into(), endpoint: Some("https://models.example".into()) };
        let selection = state.settings.text_model.clone();
        state.pending_models = Some(selection.clone());
        state.accept(AiSnapshot { available: true, busy: true, stage: "discovering".into(), ..Default::default() });
        state.accept(AiSnapshot { available: true, stage: "completed".into(), models: vec!["text-a".into(), "text-b".into()], ..Default::default() });
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 1200.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        panel.get_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).click();
        panel.run_steps(2);
        panel.get_by_label("text-a").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.text_model.model, "text-a");
        panel.state_mut().accept(AiSnapshot { available: true, stage: "completed".into(), ..Default::default() });
        panel.run_steps(3);
        panel.get_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).click();
        panel.run_steps(2);
        panel.get_by_label("text-b").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.text_model.model, "text-b");
        panel.state_mut().settings.text_model.endpoint = Some("https://other.example".into());
        panel.run_steps(3);
        assert!(
            panel.query_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).is_none()
        );
        panel.state_mut().settings.text_model.endpoint = selection.endpoint;
        panel.state_mut().settings.text_model.provider = ProviderKind::Ollama;
        panel.run_steps(3);
        assert!(
            panel.query_by(|node| node.role() == egui::accesskit::Role::ComboBox && node.value().as_deref() == Some("Available models")).is_none()
        );
    }

    #[test]
    fn ai20_ui_natural_book_estimates_preserve_review_size_remaining_pages_and_explicit_budget() {
        let mut state = AiUiState { prompt: "Create a clear science book for children.".into(), pages: 12, ..Default::default() };
        state.settings.text_model.model = "fixture".into();
        state.settings.pages_per_batch = 6;
        let estimate = state.request_estimate(&[], None).expect("natural book briefs need a conservative planning allowance");
        assert_eq!(estimate.review_pages, 6);
        assert_eq!(estimate.allowance(), 6);
        let original_tokens = state.settings.budget.token_limit;
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 1200.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 4);
        assert!(panel.query_by_label("Configured request limit 4 is below the minimum text requests for this review: 6.").is_none());
        panel.get_by_label("Set request allowance for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 6);
        assert_eq!(panel.state().settings.pages_per_batch, 6);
        assert_eq!(panel.state().pages, 12);
        assert_eq!(panel.state().settings.budget.token_limit, original_tokens);

        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":48,"facingPages":false})).unwrap();
        for spread in 0..12 {
            session.execute("frame.create", &json!({"spread":spread,"rect":[20,20,100,80]})).unwrap();
        }
        let document = OpenDocument::from_document(3, session.active().unwrap());
        let mut continuing =
            AiUiState { prompt: "Finish the remaining science lessons.".into(), new_document: false, continue_book: true, ..Default::default() };
        continuing.settings.pages_per_batch = 40;
        continuing.settings.output_mode = OutputMode::NativeWithImages;
        continuing.settings.verify_generated_images = true;
        let estimate = continuing.request_estimate(&[document], Some(3)).unwrap();
        assert_eq!(estimate.review_pages, 36);
        assert_eq!(estimate.image_allowance, 36);
        assert_eq!(estimate.vision_allowance, 36);
        assert_eq!(estimate.allowance(), 108);
        assert_eq!(continuing.settings.pages_per_batch, 40);
        assert_eq!(continuing.settings.budget.max_requests, 4);
        continuing.continue_book = false;
        assert!(continuing.request_estimate(&[], None).is_none(), "ordinary edits do not use book planning estimates");
    }

    #[test]
    fn ai20_ui_binding_choices_are_visible_and_existing_book_binding_is_authoritative() {
        let mut state = AiUiState { prompt: "Create a bilingual facing-page book.".into(), ..Default::default() };
        state.settings.text_model.model = "fixture".into();
        state.settings.facing_pages = true;
        state.settings.right_to_left_binding = Some(false);
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1300.0, 1400.0))
            .build_ui_state(|ui, (state, requests)| production(ui, state, false, true, &[], &mut None, requests), (state, Vec::new()));
        panel.run_steps(3);
        assert!(panel.query_by_label("Left to right (LTR)").is_some());
        assert!(
            panel.query_by_label("Automatic binding: Arabic reads right to left; English reads left to right. The cover stays single.").is_none()
        );
        for (label, expected) in [("Automatic (content language)", None), ("Right to left (RTL)", Some(true)), ("Left to right (LTR)", Some(false))] {
            panel.get_by_label(label).click();
            panel.run_steps(3);
            assert_eq!(panel.state().0.settings.right_to_left_binding, expected);
            panel.get_by_label("Produce").click();
            panel.run_steps(3);
            assert!(
                matches!(panel.state().1.last(), Some(AiRequest::Start(request)) if request.settings.right_to_left_binding == expected && request.settings.facing_pages)
            );
        }

        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":4,"facingPages":true})).unwrap();
        session.execute("layout.documentSetup", &json!({"binding":"rightToLeft"})).unwrap();
        session.execute("frame.create", &json!({"spread":0,"rect":[20,20,100,80]})).unwrap();
        let documents = vec![OpenDocument::from_document(0, session.active().unwrap())];
        let mut state = AiUiState { prompt: "Continue this existing book.".into(), new_document: false, continue_book: true, ..Default::default() };
        state.settings.right_to_left_binding = Some(false);
        let mut existing = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1300.0, 1400.0))
            .build_ui_state(move |ui, state| production(ui, state, false, true, &documents, &mut Some(0), &mut Vec::new()), state);
        existing.run_steps(3);
        assert!(existing.query_by_label("Existing book binding: Right to left (RTL)").is_some());
        assert_eq!(existing.get_by_label("Facing pages").accesskit_node().toggled(), Some(egui::accesskit::Toggled::True));
        assert!(existing.get_by_label("Facing pages").accesskit_node().is_disabled());
        assert!(existing.query_by_label("Automatic (content language)").is_none());
        assert_eq!(
            existing.state().settings.right_to_left_binding,
            Some(false),
            "viewing the existing binding does not rewrite new-book preferences"
        );
        assert!(!existing.state().settings.facing_pages, "displaying preserved geometry does not rewrite new-book preferences");
        existing.state_mut().continue_book = false;
        existing.run_steps(3);
        assert_eq!(existing.get_by_label("Facing pages").accesskit_node().toggled(), Some(egui::accesskit::Toggled::True));
        assert!(existing.get_by_label("Facing pages").accesskit_node().is_disabled());
    }

    #[test]
    fn ai20_ui_document_and_skill_export_destinations_are_independent_and_session_only() {
        let mut state = AiUiState { export_path: "book.pdf".into(), ..Default::default() };
        state.snapshot.review = Some(designcraft_ai_types::protocol::SkillReview {
            id: "fixture-skill".into(),
            revision: "revision-one".into(),
            skill_md: String::new(),
            manifest_json: String::new(),
            sample_path: None,
            sample_valid: false,
            files: Vec::new(),
        });
        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1300.0, 1800.0)).build_ui_state(
            |ui, (state, requests, skill_tab)| {
                if *skill_tab {
                    skills(ui, state, false, true, false, requests);
                } else {
                    export_print(ui, state, false, true, requests);
                }
            },
            (state, Vec::new(), true),
        );
        panel.run_steps(3);
        panel.get_by(|node| node.role() == egui::accesskit::Role::TextInput && node.value().as_deref() == Some("skill-package.zip")).focus();
        panel.run_steps(1);
        panel.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::A);
        panel.event(egui::Event::Text("portable.zip".into()));
        panel.run_steps(3);
        assert_eq!(panel.state().0.export_path, "book.pdf");
        panel.get_by_label("Export skill").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::ExportSkill { path, .. }) if path == "portable.zip"));
        panel.state_mut().2 = false;
        panel.run_steps(3);
        panel.get_by_label("Export IDML").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::ExportIdml { path }) if path == "book.idml"));
        panel.state_mut().2 = true;
        panel.run_steps(3);
        assert!(panel.query_by(|node| node.role() == egui::accesskit::Role::TextInput && node.value().as_deref() == Some("portable.zip")).is_some());
        let ui = crate::UiState { ai: panel.state().0.clone(), ..Default::default() };
        let saved = serde_json::to_string(&ui).unwrap();
        assert!(!saved.contains("portable.zip") && !saved.contains("book.idml"));
    }

    fn continuation_request(target: TaskTarget) -> TaskRequest {
        let mut settings = AiSettings::default();
        settings.text_model.model = "continuation-fixture".into();
        settings.pages_per_batch = 3;
        TaskRequest {
            prompt: "Keep the approved opening pages and finish the original book.".into(),
            settings,
            skill_ids: vec!["original-book-style".into()],
            auto_select_skills: false,
            target,
            export_directory: Some("book-output".into()),
        }
    }

    fn saved_production_snapshot(request: &TaskRequest, document: &designcraft_engine::DocState) -> AiSnapshot {
        let mut snapshot = serde_json::to_value(AiSnapshot::default()).unwrap();
        snapshot["available"] = json!(true);
        snapshot["production_draft"] = json!({
            "id": "saved-book", "request": request, "document_uid": document.uid,
            "document_path": document.path, "document_title": document.title()
        });
        serde_json::from_value(snapshot).unwrap()
    }

    #[test]
    fn continuation_ui_control_start_populates_the_composer_without_serializing_secrets() {
        let mut app = DesignApp::new(
            Session::new(),
            Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
        );
        let request = continuation_request(TaskTarget::ContinueBook);
        app.ui.ai.api_key.0 = "private-key-fixture".into();
        app.run("ai.request", json!({"method":"start", "request": request})).unwrap();
        assert_eq!(app.ui.ai.prompt, request.prompt);
        assert!(!app.ui.ai.new_document);
        assert!(app.ui.ai.continue_book);
        assert_eq!(app.ui.ai.settings.pages_per_batch, 3);
        assert!(!app.ui.ai.automatic_skills);
        assert!(app.ui.ai.selected_skills.contains("original-book-style"));
        assert_eq!(app.ui.ai.directory, "book-output");
        let saved_ui = serde_json::to_string(&app.ui).unwrap();
        assert!(!saved_ui.contains("private-key-fixture"));
        assert!(!saved_ui.contains(&request.prompt));
        assert!(app.ui.ai.reviewed_proofs.is_empty());
        assert!(app.ui.ai.batch_approval_gesture.is_none());
    }

    #[test]
    fn continuation_ui_saved_request_hydrates_once_and_keeps_subsequent_edits() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":4})).unwrap();
        let request = continuation_request(TaskTarget::NewA4 { pages: 4 });
        let snapshot = saved_production_snapshot(&request, session.active().unwrap());
        let mut state = AiUiState::default();
        state.accept(snapshot.clone());
        assert_eq!(state.prompt, request.prompt);
        assert_eq!(state.settings.pages_per_batch, 3);
        assert_eq!(state.pages, 4);
        state.prompt = "A manual revision awaiting Produce".into();
        state.accept(snapshot);
        assert_eq!(state.prompt, "A manual revision awaiting Produce");
        assert!(state.reviewed_proofs.is_empty());
        assert!(state.batch_approval_gesture.is_none());
    }

    #[test]
    fn continuation_ui_explicit_action_keeps_current_document_edits_separate() {
        let mut state = AiUiState { new_document: false, ..Default::default() };
        state.prompt = "Edit the selected document".into();
        state.settings.text_model.model = "fixture".into();
        let documents = vec![OpenDocument {
            index: 0,
            uid: 1,
            path: None,
            title: "Book".into(),
            facing_pages: false,
            right_to_left_binding: false,
            pages: 4,
            remaining_pages: 3,
            continuable: true,
            filled_ranges: vec![[1, 1]],
            blank_ranges: vec![[2, 4]],
        }];
        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 1200.0)).build_ui_state(
            move |ui, (state, requests, selected)| production(ui, state, false, true, &documents, selected, requests),
            (state, Vec::new(), Some(0)),
        );
        panel.run_steps(3);
        panel.get_by_label("Continue remaining 3 pages").click();
        panel.run_steps(3);
        assert!(panel.state().1.is_empty(), "choosing continuation is not permission to start production");
        assert_eq!(panel.state().2, Some(0));
        assert_eq!(panel.state().0.prompt, "Edit the selected document");
        panel.get_by_label("Produce").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::Start(TaskRequest { target: TaskTarget::ContinueBook, .. }))));
        panel.get_by_label("Edit current document").click();
        panel.run_steps(3);
        panel.get_by_label("Produce").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::Start(TaskRequest { target: TaskTarget::CurrentDocument, .. }))));
    }

    #[test]
    fn continuation_ui_busy_one_shot_keeps_failed_book_in_previous_task() {
        let state = AiUiState {
            snapshot: AiSnapshot { busy: true, batch: Some(BatchSnapshot { status: "failed".into(), ..Default::default() }), ..Default::default() },
            ..Default::default()
        };
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 1200.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        assert!(panel.query_by_label("Previous task").is_some());
        assert!(panel.query_by_label("Page proof review").is_none(), "a failed old book is not the running one-shot task");
    }

    #[test]
    fn continuation_ui_saved_book_preparation_requires_a_fresh_produce() {
        let mut app = DesignApp::new(Session::new(), Services::default());
        app.run("file.new", json!({"pages":4,"facingPages":false})).unwrap();
        app.run("frame.create", json!({"spread":0,"rect":[20,20,100,80]})).unwrap();
        let request = continuation_request(TaskTarget::NewA4 { pages: 4 });
        app.ui.ai.accept(saved_production_snapshot(&request, app.session.active().unwrap()));
        app.ui.ai.open = true;
        let starts = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let host_starts = starts.clone();
        let snapshot = app.ui.ai.snapshot.clone();
        app.services.ai = Some(Box::new(move |_, request| {
            if matches!(request, AiRequest::Start(_)) {
                host_starts.set(host_starts.get() + 1);
            }
            snapshot.clone()
        }));
        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1400.0, 1200.0)).build_ui_state(|ui, app| show(app, ui.ctx()), app);
        panel.run_steps(3);
        panel.get_by_label("Prepare saved book continuation").click();
        panel.run_steps(3);
        assert_eq!(starts.get(), 0);
        assert!(panel.state().ui.ai.continue_book);
        assert!(!panel.state().ui.ai.new_document);
        assert_eq!(panel.state().ui.ai.prompt, request.prompt);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        panel.get_by_label("Produce").click();
        panel.run_steps(3);
        assert_eq!(starts.get(), 1);
    }

    #[test]
    fn continuation_ui_proof_document_navigation_does_not_review_or_approve() {
        let mut fixture = ProofFixture::new();
        let proof_uid = fixture.app.session.active().unwrap().uid;
        fixture.app.run("file.new", json!({"pages":2})).unwrap();
        assert_ne!(fixture.app.session.active().unwrap().uid, proof_uid);
        let calls = fixture.calls.clone();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        panel.get_by_label("Open proof document").click();
        panel.run_steps(3);
        assert_eq!(panel.state().session.active().unwrap().uid, proof_uid);
        assert!(panel.state().ui.ai.proof_document_matches);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        assert!(!panel.state().ui.ai.can_approve_batch());
        assert_eq!(calls.get(), 0, "document navigation must not call the AI host");
    }

    #[test]
    fn continuation_ui_saved_path_precedes_reused_uid_and_restored_targets_require_choice() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":4})).unwrap();
        let request = continuation_request(TaskTarget::ContinueBook);
        let mut snapshot = saved_production_snapshot(&request, session.active().unwrap());
        let mut value = serde_json::to_value(&snapshot).unwrap();
        value["production_draft"]["document_path"] = json!("original-book.designcraft");
        snapshot = serde_json::from_value(value).unwrap();
        let mut state = AiUiState::default();
        state.accept(snapshot);
        let mut wrong = OpenDocument::from_document(0, session.active().unwrap());
        wrong.path = Some("different-book.designcraft".into());
        wrong.continuable = true;
        let documents = [wrong];
        assert!(saved_book_document(state.snapshot.production_draft.as_ref().unwrap(), &documents, Some(0)).is_none());
        assert_eq!(
            state.production_issue(true, true, false),
            Some("Choose the target book or prepare its remaining pages before producing the saved request.")
        );
    }

    #[test]
    fn continuation_ui_retry_requires_live_retryability_and_exact_selected_document() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":4})).unwrap();
        let document = session.active().unwrap();
        let request = continuation_request(TaskTarget::ContinueBook);
        let mut snapshot = saved_production_snapshot(&request, document);
        snapshot.batch = Some(BatchSnapshot {
            job_id: "saved-book".into(),
            status: "failed".into(),
            retryable: true,
            document_uid: Some(document.uid),
            document_revision: Some(document.revision),
            ..Default::default()
        });
        let mut state = AiUiState::default();
        state.accept(snapshot);
        let documents = vec![OpenDocument::from_document(0, document)];
        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1200.0, 1200.0)).build_ui_state(
            |ui, (state, requests, selected, documents)| production(ui, state, false, true, documents, selected, requests),
            (state, Vec::new(), Some(0), documents),
        );
        panel.run_steps(3);
        assert!(!panel.get_by_label("Retry failed review group").accesskit_node().is_disabled());
        panel.get_by_label("Retry failed review group").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::RetryBatch { job_id }) if job_id == "saved-book"));
        let batch = panel.state_mut().0.snapshot.batch.as_mut().unwrap();
        batch.document_revision = batch.document_revision.map(|revision| revision + 1);
        panel.run_steps(3);
        assert!(
            !panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(),
            "view revisions do not change the selected book's identity"
        );
        panel.state_mut().3.first_mut().unwrap().uid += 1;
        panel.run_steps(3);
        assert!(panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(), "a different book cannot retry this task");
        panel.state_mut().2 = None;
        panel.run_steps(3);
        assert!(panel.get_by_label("Retry failed review group").accesskit_node().is_disabled());
        panel.state_mut().0.snapshot.batch.as_mut().unwrap().retryable = false;
        panel.run_steps(3);
        assert!(panel.query_by_label("Retry failed review group").is_none(), "a persisted failure never grants retry authority");
        assert!(panel.state().0.reviewed_proofs.is_empty());
        assert!(panel.state().0.batch_approval_gesture.is_none());
    }

    #[test]
    fn continuation_ui_unpublished_new_book_retry_does_not_require_an_open_document() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"pages":4})).unwrap();
        let request = continuation_request(TaskTarget::NewA4 { pages: 4 });
        let mut snapshot = saved_production_snapshot(&request, session.active().unwrap());
        snapshot.production_draft.as_mut().unwrap().document_uid = None;
        snapshot.batch = Some(BatchSnapshot { job_id: "saved-book".into(), status: "failed".into(), retryable: true, ..Default::default() });
        let mut state = AiUiState::default();
        state.accept(snapshot);
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1200.0, 1200.0))
            .build_ui_state(|ui, (state, requests)| production(ui, state, false, true, &[], &mut None, requests), (state, Vec::new()));
        panel.run_steps(3);
        assert!(!panel.get_by_label("Retry failed review group").accesskit_node().is_disabled());
        panel.get_by_label("Retry failed review group").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::RetryBatch { job_id }) if job_id == "saved-book"));
        panel.state_mut().0.snapshot.production_draft.as_mut().unwrap().request.target = TaskTarget::ContinueBook;
        panel.run_steps(3);
        assert!(
            panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(),
            "an existing book still needs an exact selected document"
        );
        panel.state_mut().0.snapshot.production_draft.as_mut().unwrap().request.target = TaskTarget::NewA4 { pages: 4 };
        panel.state_mut().0.snapshot.batch.as_mut().unwrap().completed_pages = 1;
        panel.run_steps(3);
        assert!(panel.get_by_label("Retry failed review group").accesskit_node().is_disabled(), "published pages cannot use document-free retry");
    }

    #[test]
    fn continuation_ui_view_only_revision_requests_one_repin_and_preserves_exact_proof_review() {
        let mut fixture = ProofFixture::new();
        fixture.confirm_all();
        let original = fixture.app.session.active().unwrap().doc.clone();
        let pinned = original.clone();
        let mut checkpoint = fixture.checkpoint.clone();
        let polls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let host_polls = polls.clone();
        fixture.app.services.ai = Some(Box::new(move |session, request| {
            assert!(matches!(request, AiRequest::Poll), "revision validation cannot approve or start work");
            host_polls.set(host_polls.get() + 1);
            let current = session.active().unwrap();
            if std::sync::Arc::ptr_eq(&current.doc, &pinned) {
                checkpoint.batch.as_mut().unwrap().document_revision = Some(current.revision);
            }
            checkpoint.clone()
        }));
        let mut panel = fixture.panel();
        panel.run_steps(3);
        let reviewed = panel.state().ui.ai.reviewed_proofs.clone();
        let proof_path = panel.state().ui.ai.proof_path.clone();
        assert_eq!(reviewed.len(), 2);
        for (expected_polls, command, params) in [(1, "selection.set", json!({"ids":[]})), (2, "layout.selectPage", json!({"page":1}))] {
            let before = panel.state().session.active().unwrap().revision;
            panel.state_mut().run(command, params).unwrap();
            assert_ne!(panel.state().session.active().unwrap().revision, before);
            assert!(std::sync::Arc::ptr_eq(&panel.state().session.active().unwrap().doc, &original));
            panel.run_steps(5);
            assert_eq!(polls.get(), expected_polls, "view-only revision changes request one local validation");
            assert_eq!(panel.state().ui.ai.reviewed_proofs, reviewed);
            assert_eq!(panel.state().ui.ai.proof_path, proof_path);
            assert!(panel.state().ui.ai.can_approve_batch());
            assert!(panel.state().ui.ai.batch_approval_gesture.is_none());
        }
        panel.state_mut().run("frame.create", json!({"spread":0,"rect":[20,20,80,80]})).unwrap();
        panel.run_steps(5);
        assert_eq!(polls.get(), 3);
        assert!(!panel.state().ui.ai.proof_document_matches);
        assert!(!panel.state().ui.ai.can_approve_batch(), "an actual content edit cannot be repinned by the host");
        panel.run_steps(5);
        assert_eq!(polls.get(), 3, "a rejected content revision must not trigger repeated validation polls");
        assert!(panel.state().ui.ai.batch_approval_gesture.is_none());
    }

    #[test]
    fn continuation_ui_changed_view_can_return_to_its_book_before_revision_validation() {
        let mut fixture = ProofFixture::new();
        let original = fixture.app.session.active().unwrap().clone();
        let uid = original.uid;
        let mut checkpoint = fixture.checkpoint.clone();
        let polls = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let host_polls = polls.clone();
        fixture.app.services.ai = Some(Box::new(move |session, request| {
            assert!(matches!(request, AiRequest::Poll));
            host_polls.set(host_polls.get() + 1);
            let current = session.active().unwrap();
            if current.uid == uid && std::sync::Arc::ptr_eq(&current.doc, &original.doc) {
                checkpoint.batch.as_mut().unwrap().document_revision = Some(current.revision);
            }
            checkpoint.clone()
        }));
        fixture.app.run("selection.set", json!({"ids":[]})).unwrap();
        fixture.app.run("file.new", json!({"pages":2})).unwrap();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        assert_eq!(polls.get(), 0);
        panel.get_by_label("Open proof document").click();
        panel.run_steps(5);
        assert_eq!(panel.state().session.active().unwrap().uid, uid);
        assert_eq!(polls.get(), 1);
        assert!(panel.state().ui.ai.proof_document_matches);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        assert!(!panel.state().ui.ai.can_approve_batch(), "returning to the book never reviews its pages");
        assert!(panel.state().ui.ai.batch_approval_gesture.is_none());
    }
    #[test]
    fn production_composer_exposes_model_prompt_and_action_without_advanced_settings() {
        let mut app = DesignApp::new(Session::new(), Services::default());
        app.ui.ai.open = true;
        app.ui.ai.snapshot.available = true;
        app.ui.ai.initialized = true;
        app.ui.ai.prompt = "A long request\n".repeat(200);
        let mut harness = crate::test_window::open(app, egui::vec2(1200.0, 900.0));
        harness.run_steps(3);
        assert!(harness.query_by_label("Model or agent").is_some());
        assert!(harness.query_by_label("What would you like to create?").is_some());
        assert!(harness.query_by_label("Produce").is_some());
        assert!(harness.get_by_label("Produce").rect().max.y < 650.0, "long briefs must not push the action off screen");
        assert!(harness.query_by_label("Input").is_none(), "pricing belongs in advanced settings");
    }
    #[test]
    fn disabled_production_button_explains_the_missing_model() {
        let mut app = DesignApp::new(
            Session::new(),
            Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
        );
        app.run("ai.production", json!({})).unwrap();
        app.ui.ai.prompt = "Create an editable activity".into();
        let mut harness = crate::test_window::open(app, egui::vec2(1500.0, 1100.0));
        harness.run_steps(3);
        assert!(harness.get_by_label("Produce").accesskit_node().is_disabled());
        assert!(harness.query_by_label("Choose a text model above.").is_some(), "a disabled button must explain what to fix");
        harness.state_mut().app.ui.ai.settings.text_model.model = "fixture-model".into();
        harness.run_steps(3);
        assert!(!harness.get_by_label("Produce").accesskit_node().is_disabled());
        assert!(harness.query_by_label("Choose a text model above.").is_none());
    }

    #[test]
    fn production_readiness_preserves_workflow_budget_and_human_review_guards() {
        let mut state = AiUiState { prompt: "Native activity".into(), ..Default::default() };
        state.settings.text_model.model = "text-model".into();
        state.settings.output_mode = OutputMode::NativeWithImages;
        state.settings.image_model.provider = ProviderKind::Gemini;
        assert_eq!(state.production_issue(true, false, false), Some("Choose an image model in Providers and keys."));
        state.settings.image_model.model = "image-model".into();
        state.settings.budget.max_cost_usd = Some(1.8);
        assert_eq!(state.production_issue(true, false, true), Some("أدخل أسعار المزود وأكّدها من الإعدادات المتقدمة."));
        state.rates_confirmed = true;
        assert_eq!(state.production_issue(true, false, false), None);
        state.settings.image_model.provider = ProviderKind::ComfyUi;
        state.settings.image_model.model.clear();
        assert_eq!(state.production_issue(true, false, false), Some("Complete a valid ComfyUI API workflow in Providers and keys."));
        state.workflow_json = r#"{"node":{"text":"{{prompt}}"}}"#.into();
        state.edit_workflow();
        assert_eq!(state.production_issue(true, false, false), None, "a workflow can specify its checkpoint without a model placeholder");
        state.snapshot.batch = Some(BatchSnapshot { status: "awaiting_human_proof".into(), ..Default::default() });
        assert_eq!(
            state.production_issue(true, false, false),
            Some("Review and approve the current batch, or cancel the task before starting another.")
        );
        assert!(!state.can_approve_batch());
    }

    #[test]
    fn explicit_page_review_estimate_changes_request_allowance_only_after_the_button() {
        let mut state = AiUiState { prompt: (1..=8).map(|page| format!("### PAGE {page:02} — Lesson {page}\n")).collect(), ..Default::default() };
        state.pages = 8;
        state.settings.pages_per_batch = 6;
        state.settings.text_model.model = "fixture-model".into();
        let token_limit = state.settings.budget.token_limit;
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 1100.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        assert!(panel.query_by_label("Requests for this review (estimate): 6 text + 0 images = 6").is_some());
        assert!(panel.query_by_label("Configured request limit 4 is below the minimum text requests for this review: 6.").is_some());
        assert_eq!(panel.state().settings.budget.max_requests, 4, "an estimate never raises the spending allowance itself");
        panel.get_by_label("Set request allowance for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 6);
        assert_eq!(panel.state().settings.pages_per_batch, 6);
        assert_eq!(panel.state().pages, 8);
        assert_eq!(panel.state().settings.budget.token_limit, token_limit);

        panel.state_mut().settings.output_mode = OutputMode::NativeWithImages;
        panel.run_steps(3);
        assert!(panel.query_by_label("Requests for this review (estimate): 6 text + 6 images = 12").is_some());
        assert_eq!(panel.state().settings.budget.max_requests, 6, "provisional image allowance requires another explicit choice");
        panel.get_by_label("Set request allowance for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 12);
        assert_eq!(panel.state().settings.pages_per_batch, 6);
        panel.state_mut().settings.text_model.provider = ProviderKind::Gemini;
        panel.state_mut().settings.verify_generated_images = true;
        panel.run_steps(3);
        assert!(panel.query_by_label("Requests for this review (estimate): 6 text + 6 images + 6 vision checks = 18").is_some());
        assert_eq!(panel.state().settings.budget.max_requests, 12, "opting into inspection still does not raise request allowance implicitly");
        panel.get_by_label("Set request allowance for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().settings.budget.max_requests, 18);
        assert_eq!(panel.state().settings.pages_per_batch, 6);
        assert_eq!(panel.state().settings.budget.token_limit, token_limit);
    }

    #[test]
    fn image_inspection_requires_explicit_gemini_choice_and_can_be_disabled_after_provider_change() {
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(900.0, 400.0))
            .build_ui_state(|ui, state| image_inspection_setting(ui, state, false), AiUiState::default());
        let label = "Inspect generated images with Gemini (extra billed vision requests)";
        panel.run_steps(3);
        assert!(panel.get_by_label(label).accesskit_node().is_disabled());
        assert!(!panel.state().settings.verify_generated_images);
        panel.state_mut().settings.text_model.provider = ProviderKind::Gemini;
        panel.run_steps(3);
        assert!(!panel.get_by_label(label).accesskit_node().is_disabled());
        panel.get_by_label(label).click();
        panel.run_steps(3);
        assert!(panel.state().settings.verify_generated_images);
        assert_eq!(panel.state().settings.budget.max_requests, 4);
        assert_eq!(panel.state().settings.pages_per_batch, 2);
        panel.state_mut().settings.text_model.provider = ProviderKind::Ollama;
        panel.run_steps(3);
        assert!(panel.get_by_label(label).accesskit_node().is_disabled());
        assert!(panel.state().settings.verify_generated_images, "provider changes do not silently change an explicit inspection choice");
        panel.get_by_label("Turn off image inspection").click();
        panel.run_steps(3);
        assert!(!panel.state().settings.verify_generated_images);
        assert_eq!(panel.state().settings.budget.max_requests, 4);
        assert_eq!(panel.state().settings.pages_per_batch, 2);
    }

    #[test]
    fn continuation_request_estimate_counts_only_remaining_pages_and_ignores_fenced_examples() {
        let prompt = format!(
            "```markdown\n### PAGE 99 — Example only\n```\n{}",
            (1..=8).map(|page| format!("### PAGE {page:02} — Lesson {page}\n")).collect::<String>()
        );
        let mut state = AiUiState { prompt, new_document: false, continue_book: true, ..Default::default() };
        state.settings.pages_per_batch = 6;
        state.settings.output_mode = OutputMode::NativeWithImages;
        let document = OpenDocument {
            index: 3,
            uid: 1,
            path: None,
            title: "Book".into(),
            facing_pages: false,
            right_to_left_binding: false,
            pages: 8,
            remaining_pages: 2,
            continuable: true,
            filled_ranges: vec![[1, 6]],
            blank_ranges: vec![[7, 8]],
        };
        let estimate = state.request_estimate(&[document], Some(3)).unwrap();
        assert_eq!(estimate.review_pages, 2);
        assert_eq!(estimate.image_allowance, 2);
        assert_eq!(state.settings.budget.max_requests, 4);
    }

    #[test]
    fn facing_page_choice_preserves_automatic_binding_without_another_direction_prompt() {
        let state = AiUiState::default();
        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1000.0, 1100.0))
            .build_ui_state(|ui, state| production(ui, state, false, true, &[], &mut None, &mut Vec::new()), state);
        panel.run_steps(3);
        assert!(!panel.state().settings.facing_pages);
        panel.get_by_label("Facing pages").click();
        panel.run_steps(3);
        assert!(panel.state().settings.facing_pages);
        assert_eq!(panel.state().settings.right_to_left_binding, None);
        assert!(
            panel.query_by_label("Automatic binding: Arabic reads right to left; English reads left to right. The cover stays single.").is_some()
        );
    }
    #[test]
    fn facing_book_continuation_counts_blank_pages_inside_a_partly_filled_pair() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"width":360,"height":480,"pages":5,"facingPages":true})).unwrap();
        let add_page_item = |session: &mut Session, absolute| {
            let document = session.doc().unwrap();
            let (spread, page) = document.doc.page_loc(absolute).unwrap();
            let x = document.doc.spreads[spread].pages[page].x;
            session.execute("frame.create", &json!({"spread":spread,"rect":[x+20.0,20.0,x+100.0,80.0]})).unwrap();
        };
        add_page_item(&mut session, 0);
        add_page_item(&mut session, 1);
        assert!(session.doc().unwrap().doc.spreads.iter().any(|spread| spread.pages.len() == 2));
        let book = OpenDocument::from_document(0, session.doc().unwrap());
        assert!(book.continuable);
        assert_eq!(book.pages, 5);
        assert_eq!(book.remaining_pages, 3, "the blank right page of the first pair is part of the suffix");
        add_page_item(&mut session, 3);
        assert!(!OpenDocument::from_document(0, session.doc().unwrap()).continuable, "a later filled page cannot bypass a blank gap");
    }

    #[test]
    fn automatic_proof_binding_uses_print_content_metadata_before_instruction_language() {
        assert!(!automatic_proof_binding("صمّم كتاباً للأطفال\nContent Copy Language: English", true));
        assert!(automatic_proof_binding("Script Language: English\nContent Copy Language: English LTR + Vocalized Arabic RTL", false));
        assert!(!automatic_proof_binding("تعليمات عربية: محتوى الكتاب باللغة الإنجليزية", true));
        assert!(automatic_proof_binding("Binding: RTL\nContent Copy Language: English", false));
    }

    #[test]
    fn long_bilingual_composer_keeps_the_full_brief_and_uses_the_shared_limit() {
        let mut state = AiUiState { prompt: "تعليمات المنهج Mathematics\n".repeat(4000), ..Default::default() };
        state.settings.text_model.model = "fixture-model".into();
        let original = state.prompt.clone();
        assert!(original.len() > 32768);
        assert_eq!(state.production_issue(true, false, false), None);
        assert_eq!(state.prompt, original);
        state.prompt = "x".repeat(designcraft_ai_types::protocol::MAX_BRIEF_BYTES + 1);
        assert!(state.production_issue(true, false, false).is_some());
    }
    #[test]
    fn local_resource_preset_keeps_the_requested_human_review_size() {
        let mut app = DesignApp::new(
            Session::new(),
            Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
        );
        app.ui.ai.open = true;
        app.ui.ai.initialized = true;
        app.ui.ai.snapshot.available = true;
        app.ui.ai.tab = 4;
        app.ui.ai.settings.pages_per_batch = 12;
        let mut harness = crate::test_window::open(app, egui::vec2(1800.0, 1500.0));
        harness.run_steps(3);
        harness.get_by_label("Conservative CPU (Ollama)").click();
        harness.run_steps(3);
        assert_eq!(harness.state().app.ui.ai.settings.pages_per_batch, 12);
        assert_eq!(harness.state().app.ui.ai.settings.resources.cpu_threads, 2);
    }
    #[test]
    fn opened_document_target_is_visible_without_creating_or_resizing_it() {
        let mut session = Session::new();
        session.execute("file.new", &json!({"width":360,"height":480,"pages":3,"facingPages":false})).unwrap();
        let mut app = DesignApp::new(session, Services::default());
        app.ui.ai.open = true;
        app.ui.ai.initialized = true;
        app.ui.ai.new_document = false;
        let mut harness = crate::test_window::open(app, egui::vec2(1200.0, 900.0));
        harness.run_steps(3);
        assert!(harness.query_by_label("Target document").is_some());
        let state = harness.state().app.session.doc().unwrap();
        assert_eq!(state.doc.page_count(), 3);
        assert_eq!(state.doc.settings.page_width, 360.0);
        assert!(!state.is_dirty());
    }

    #[test]
    fn requested_large_review_group_keeps_every_page_in_the_human_proof() {
        let fixture = ProofFixture::new();
        let mut batch = fixture.checkpoint.batch.clone().unwrap();
        batch.total_pages = 64;
        batch.pages_per_batch = 64;
        batch.total_batches = 1;
        batch.completed_pages = 64;
        let report = &mut batch.batches[0];
        report.page_count = 64;
        report.proof_paths = vec!["document.designcraft".into(), "proof-1.pdf".into(), "proof-2.pdf".into()];
        report.proof_paths.extend((1..=64).map(|page| format!("page-{page}.png")));
        report.proof_sha256 = vec!["a".repeat(64); report.proof_paths.len()];
        let key = BatchReviewKey::from_batch(&batch).expect("A complete 64-page review group must not be rejected by an internal 32-page limit");
        assert_eq!(key.pages().count(), 64);
        let report = &mut batch.batches[0];
        report.proof_paths.pop();
        report.proof_sha256.pop();
        assert!(BatchReviewKey::from_batch(&batch).is_none(), "Every requested page still needs its own proof");
    }

    #[test]
    fn callback_dispatch_and_manual_settings_survive_poll() {
        let mut app = DesignApp::new(
            Session::new(),
            Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
        );
        app.run("ai.production", json!({})).unwrap();
        app.ui.ai.settings.text_model.model = "manual-model".into();
        app.run("ai.request", json!({"method":"poll"})).unwrap();
        assert_eq!(app.ui.ai.settings.text_model.model, "manual-model");
        assert!(app.ui.ai.open);
    }
    #[test]
    fn session_key_drafts_and_prompts_never_serialize() {
        let mut ui = crate::UiState::default();
        ui.ai.api_key.0 = "secret-fixture".into();
        ui.ai.prompt = "private-prompt".into();
        let serialized = serde_json::to_string(&ui).unwrap();
        assert!(!serialized.contains("secret-fixture"));
        assert!(!serialized.contains("private-prompt"));
        assert!(!format!("{:?}", ui.ai).contains("secret-fixture"));
    }
    #[test]
    fn approval_requires_valid_review_and_host_absence_is_an_error() {
        let mut app = DesignApp::new(Session::new(), Services::default());
        assert!(app.run("ai.request", json!({"method":"approve_skill", "id":"x", "revision":"1"})).is_err());
        assert!(dispatch(&mut app, AiRequest::Poll).is_err());
    }

    #[test]
    fn partial_workflow_draft_survives_poll_and_becomes_runnable_when_completed() {
        let mut state = AiUiState::default();
        state.accept(AiSnapshot { available: true, ..Default::default() });
        state.settings.output_mode = OutputMode::NativeWithImages;
        state.settings.comfy_workflow = Some(json!({"stale": true}));
        state.workflow_json = r#"{"node":{"text":"{{prompt}}""#.into();
        state.edit_workflow();
        let partial = state.workflow_json.clone();
        assert!(!state.workflow_error.is_empty());
        assert!(state.settings.comfy_workflow.is_none());
        assert!(!state.workflow_ready());
        state.accept(AiSnapshot { available: true, ..Default::default() });
        assert_eq!(state.workflow_json, partial);
        assert!(!state.workflow_error.is_empty());
        state.workflow_json.push_str(r#", "model":"{{model}}"}}"#);
        state.edit_workflow();
        assert!(state.workflow_error.is_empty());
        assert_eq!(state.settings.comfy_workflow, Some(json!({"node":{"text":"{{prompt}}", "model":"{{model}}"}})));
        assert!(state.workflow_ready());
        state.accept(AiSnapshot { available: true, ..Default::default() });
        assert_eq!(state.settings.comfy_workflow, Some(json!({"node":{"text":"{{prompt}}", "model":"{{model}}"}})));
    }

    #[test]
    fn automation_cannot_approve_even_a_valid_review_or_call_the_host() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let host_calls = calls.clone();
        let mut app = DesignApp::new(
            Session::new(),
            Services {
                ai: Some(Box::new(move |_, _| {
                    host_calls.set(host_calls.get() + 1);
                    AiSnapshot { available: true, ..Default::default() }
                })),
                ..Default::default()
            },
        );
        app.ui.ai.snapshot.available = true;
        app.ui.ai.snapshot.review = Some(designcraft_ai_types::protocol::SkillReview {
            id: "fixture".into(),
            revision: "1".into(),
            skill_md: "reviewed".into(),
            manifest_json: "{}".into(),
            sample_path: Some("sample.png".into()),
            sample_valid: true,
            files: vec![],
        });
        assert!(app.run("ai.request", json!({"method":"approve_skill", "id":"fixture", "revision":"1"})).is_err());
        app.ui.ai.snapshot.batch = Some(designcraft_ai_types::protocol::BatchSnapshot {
            job_id: "batch-fixture".into(),
            status: "awaiting_human_proof".into(),
            checkpoint: 7,
            ..Default::default()
        });
        assert!(app.run("ai.request", json!({"method":"approve_batch", "job_id":"batch-fixture", "checkpoint":7})).is_err());
        assert!(
            app.run("ai.request", json!({"method":"remove_api_key", "selection":{"provider":"open_ai","model":"fixture","endpoint":null}})).is_err()
        );
        assert!(app.run("ai.request", json!({"method":"refresh_credentials"})).is_err());
        app.ui.ai.snapshot.print_proof = Some(designcraft_ai_types::protocol::PrintProof {
            token: "frozen-fixture".into(),
            sha256: "fixture-hash".into(),
            bytes: 64,
            pages: 1,
            manifest: json!({}),
            pdf_path: Some("fixture.pdf".into()),
        });
        assert!(app.run("ai.request", json!({"method":"submit_prepared_print", "token":"frozen-fixture", "request_id":"fixture-print"})).is_err());
        assert!(app.run("ai.request", json!({"method":"open_print_proof", "token":"frozen-fixture"})).is_err());
        assert!(app.run("ai.request", json!({"method":"start_local_ollama", "executable":"C:/fixture/ollama.exe"})).is_err());
        assert!(app.run("ai.request", json!({"method":"stop_local_ollama"})).is_err());
        assert_eq!(calls.get(), 0);
    }

    struct ProofFixture {
        app: DesignApp,
        ctx: egui::Context,
        checkpoint: AiSnapshot,
        calls: std::rc::Rc<std::cell::Cell<u32>>,
    }
    #[test]
    fn ai24_recovered_proofs_remain_visible_without_restoring_approval_authority() {
        let mut fixture = ProofFixture::new();
        let batch = fixture.checkpoint.batch.as_mut().unwrap();
        batch.status = "interrupted".into();
        batch.checkpoint = 0;
        batch.document_uid = None;
        batch.document_revision = None;
        fixture.app.ui.ai.accept(fixture.checkpoint.clone());
        assert_eq!(fixture.app.ui.ai.proof_path.as_deref(), Some("page-1.png"));
        assert!(fixture.app.ui.ai.proof_key.is_none());
        assert!(fixture.app.ui.ai.select_proof("page-2.png"));
        fixture.view("page-2.png");
        fixture.app.ui.ai.review_selected_proof();
        assert!(fixture.app.ui.ai.reviewed_proofs.is_empty());
        assert!(!fixture.app.ui.ai.can_review_selected_proof());
        assert_eq!(fixture.calls.get(), 0);
    }
    #[test]
    fn ai24_saved_proof_navigation_and_return_to_composer_do_not_approve_or_generate() {
        let mut fixture = ProofFixture::new();
        let batch = fixture.checkpoint.batch.as_mut().unwrap();
        batch.status = "interrupted".into();
        batch.document_uid = None;
        batch.document_revision = None;
        fixture.app.ui.ai.accept(fixture.checkpoint.clone());
        let calls = fixture.calls.clone();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        assert!(panel.query_by_label("Selected page proof").is_some());
        assert!(panel.query_by_label("Approve and generate next review group").is_none());
        panel.get_by_label("Next").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-2.png"));
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        panel.get_by_label("Return to production settings").click();
        panel.run_steps(3);
        assert!(panel.query_by_label("What would you like to create?").is_some());
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn ai24_failed_unpublished_group_explains_why_there_are_no_proofs_to_approve() {
        let mut fixture = ProofFixture::new();
        let batch = fixture.checkpoint.batch.as_mut().unwrap();
        batch.status = "failed".into();
        batch.batches[0].proof_paths.clear();
        batch.batches[0].proof_sha256.clear();
        fixture.app.ui.ai.accept(fixture.checkpoint);
        let mut panel =
            egui_kittest::Harness::builder().build_ui_state(|ui, state| batch_review(ui, state, false, &mut Vec::new()), fixture.app.ui.ai);
        panel.run_steps(3);
        assert!(panel.query_by_label("This group has no complete published page proofs. Resolve the reported error and retry the unfinished pages; there is nothing new to approve.").is_some());
        assert!(panel.query_by_label("Approve and generate next review group").is_none());
        assert!(panel.query_by_label("Selected page proof").is_none());
    }
    impl ProofFixture {
        fn new() -> Self {
            let calls = std::rc::Rc::new(std::cell::Cell::new(0u32));
            let host_calls = calls.clone();
            let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 255, 255, 255])));
            let mut png = std::io::Cursor::new(Vec::new());
            image.write_to(&mut png, image::ImageFormat::Png).unwrap();
            let png = png.into_inner();
            let mut app = DesignApp::new(
                Session::new(),
                Services {
                    read: Some(Box::new(move |_| Ok(png.clone()))),
                    ai: Some(Box::new(move |_, _| {
                        host_calls.set(host_calls.get() + 1);
                        AiSnapshot { available: true, ..Default::default() }
                    })),
                    ..Default::default()
                },
            );
            app.run("file.new", json!({"facingPages":false})).unwrap();
            let document = app.session.active().map(|d| (d.uid, d.revision)).unwrap();
            let checkpoint = AiSnapshot {
                available: true,
                batch: Some(BatchSnapshot {
                    job_id: "two-page-proof".into(),
                    total_pages: 4,
                    pages_per_batch: 2,
                    current_batch: 1,
                    total_batches: 2,
                    completed_pages: 2,
                    status: "awaiting_human_proof".into(),
                    checkpoint: 1,
                    document_uid: Some(document.0),
                    document_revision: Some(document.1),
                    batches: vec![designcraft_ai_types::protocol::BatchReport {
                        batch: 1,
                        page_count: 2,
                        proof_paths: vec!["book.designcraft".into(), "proof.pdf".into(), "page-1.png".into(), "page-2.png".into()],
                        proof_sha256: vec!["0".repeat(64), "1".repeat(64), "2".repeat(64), "3".repeat(64)],
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            };
            app.ui.ai.accept(checkpoint.clone());
            app.ui.ai.sync_proof_document(Some(document));
            app.ui.ai.review_input_is_human = true;
            Self { app, ctx: egui::Context::default(), checkpoint, calls }
        }

        fn view(&mut self, path: &str) {
            self.app.ui.ai.proof_path = Some(path.into());
            self.app.ui.ai.proof_loaded = None;
            load_proof_preview(&mut self.app, &self.ctx);
        }

        fn confirm_all(&mut self) {
            let pages: Vec<_> = self.app.ui.ai.proof_key.as_ref().unwrap().pages().map(|(path, _)| path.clone()).collect();
            for path in pages {
                self.view(&path);
                self.app.ui.ai.review_selected_proof();
            }
        }

        fn panel(mut self) -> egui_kittest::Harness<'static, DesignApp> {
            self.app.ui.ai.open = true;
            self.app.ui.ai.prompt = "A long production request\n".repeat(1000);
            egui_kittest::Harness::builder().with_size(egui::vec2(1024.0, 768.0)).build_ui_state(|ui, app| show(app, ui.ctx()), self.app)
        }
    }

    #[test]
    fn production_review_keeps_actions_beside_the_proof_and_tracks_each_displayed_page() {
        let fixture = ProofFixture::new();
        let calls = fixture.calls.clone();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        let image = panel.get_by_label("Selected page proof").rect();
        let review = panel.get_by_label("I reviewed the displayed page").rect();
        let approval = panel.get_by_label("Approve and generate next review group").rect();
        assert!(review.min.x >= image.max.x, "page confirmation belongs beside its displayed preview");
        assert!(approval.min.x >= image.max.x, "group approval belongs beside the preview");
        assert!(image.contains(egui::pos2(image.center().x, review.center().y)));
        assert!(image.contains(egui::pos2(image.center().x, approval.center().y)));
        assert!(image.max.y < 768.0 && approval.max.y < 768.0, "long requests and file lists must not require scrolling to approve");
        assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
        assert!(panel.get_by_label("Previous").accesskit_node().is_disabled());
        assert!(panel.query_by_label("Pages reviewed: 0 / 2").is_some());

        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert!(panel.query_by_label("Reviewed").is_some());
        assert_eq!(panel.state().ui.ai.reviewed_proofs, BTreeSet::from([("page-1.png".into(), "2".repeat(64))]));
        assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());

        panel.get_by_label("Next").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-2.png"));
        assert_eq!(panel.state().ui.ai.proof_loaded.as_deref(), Some("page-2.png"));
        assert!(panel.query_by_label("Pages reviewed: 1 / 2").is_some());
        assert!(panel.get_by_label("Next").accesskit_node().is_disabled());
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 1, "navigation and loading never review the next page");

        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
        assert!(!panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
        assert_eq!(calls.get(), 0, "per-page review never submits group approval");
    }

    #[test]
    fn busy_production_shows_live_usage_and_keeps_full_diagnostics_in_details() {
        let mut fixture = ProofFixture::new();
        fixture.app.ui.ai.snapshot.busy = true;
        fixture.app.ui.ai.snapshot.current_usage = designcraft_ai_types::protocol::UsageSummary {
            requests: 3,
            input_tokens: 1042,
            output_tokens: 91,
            estimated_cost_usd: Some(0.0123),
            ..Default::default()
        };
        let issue = json!({"page":7,"field":"body","reason":"Native text exceeds frame","solution":"Increase frame height"});
        fixture.app.ui.ai.snapshot.current_issues = vec![issue.clone()];
        fixture.app.ui.ai.error = "Native text exceeds frame\nPage 7 body needs another line.".into();
        let mut panel = fixture.panel();
        panel.run_steps(3);
        assert!(panel.query_by_label("Live requests: 3 / 4 · Input 1042 · Output 91 · Estimated $0.012300").is_some());
        assert!(panel.query_by_label("Native text exceeds frame").is_some());
        assert!(panel.query_by_label("Native text exceeds frame\nPage 7 body needs another line.").is_none());
        let diagnostic = serde_json::to_string_pretty(&issue).unwrap();
        assert!(panel.query_by_label(&diagnostic).is_none());
        panel.get_by_label("Details").click();
        panel.run_steps(3);
        assert!(panel.query_by_label(&diagnostic).is_some());
        assert!(panel.query_by_label("Native text exceeds frame\nPage 7 body needs another line.").is_some());
        assert!(panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled());
        assert!(panel.get_by_label("Approve and generate next review group").rect().max.y < 768.0);
    }

    #[test]
    fn facing_proofs_keep_cover_single_order_pairs_and_review_each_selected_page_separately() {
        let mut fixture = ProofFixture::new();
        let batch = fixture.checkpoint.batch.as_mut().unwrap();
        batch.total_pages = 6;
        batch.pages_per_batch = 3;
        batch.completed_pages = 3;
        let report = &mut batch.batches[0];
        report.page_count = 3;
        report.proof_paths.push("page-3.png".into());
        report.proof_sha256.push("4".repeat(64));
        fixture.app.ui.ai.accept(fixture.checkpoint.clone());
        fixture.app.ui.ai.settings.facing_pages = true;
        fixture.app.ui.ai.settings.right_to_left_binding = Some(false);
        let mut panel = fixture.panel();
        panel.run_steps(3);
        assert!(panel.query_by_label("Selected page proof").is_some(), "the first cover remains a single page");
        assert!(panel.query_by_label("Page 2 proof").is_none());
        panel.get_by_label("Next").click();
        panel.run_steps(3);
        assert!(panel.get_by_label("Page 2 proof").rect().center().x < panel.get_by_label("Page 3 proof").rect().center().x);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty(), "showing a pair reviews neither page");
        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs, BTreeSet::from([("page-2.png".into(), "3".repeat(64))]));
        panel.get_by_label("Page 3").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.proof_path.as_deref(), Some("page-3.png"));
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 1);
        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
        assert!(
            panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled(),
            "the cover still requires its own review"
        );
        panel.state_mut().ui.ai.settings.right_to_left_binding = Some(true);
        panel.run_steps(3);
        assert!(panel.get_by_label("Page 3 proof").rect().center().x < panel.get_by_label("Page 2 proof").rect().center().x);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2, "changing physical reading order cannot add a review");
        panel.get_by_label("Show facing pair").click();
        panel.run_steps(3);
        assert!(panel.query_by_label("Selected page proof").is_some());
        assert!(panel.state().ui.ai.companion_proof.is_none());
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
        panel.get_by_label("Show facing pair").click();
        panel.run_steps(3);
        assert!(panel.get_by_label("Page 3 proof").rect().center().x < panel.get_by_label("Page 2 proof").rect().center().x);
        panel.get_by_label("Previous").click();
        panel.run_steps(3);
        assert!(panel.get_by_label("Reviewed").accesskit_node().is_disabled());
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
    }

    #[test]
    fn facing_partner_outside_review_group_is_a_placeholder_without_implied_review() {
        let mut fixture = ProofFixture::new();
        fixture.app.ui.ai.settings.facing_pages = true;
        fixture.app.ui.ai.settings.right_to_left_binding = Some(false);
        let mut panel = fixture.panel();
        panel.run_steps(3);
        panel.get_by_label("Next").click();
        panel.run_steps(3);
        assert!(panel.query_by_label("Page 2 proof").is_some());
        assert!(panel.query_by_label("Page 3 proof").is_none());
        assert!(panel.query_by_label("Outside this review group").is_some());
        assert!(panel.state().ui.ai.companion_proof.is_none());
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty());
        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 1);
        panel.get_by_label("Previous").click();
        panel.run_steps(3);
        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs.len(), 2);
        assert!(
            !panel.get_by_label("Approve and generate next review group").accesskit_node().is_disabled(),
            "only actual pages in the current group are required"
        );
    }

    #[test]
    fn facing_proof_prefers_matching_document_binding_and_drops_it_after_revision_change() {
        let mut fixture = ProofFixture::new();
        fixture.app.run("layout.documentSetup", json!({"facingPages":true,"binding":"rightToLeft"})).unwrap();
        let document = fixture.app.session.active().unwrap();
        let batch = fixture.checkpoint.batch.as_mut().unwrap();
        batch.document_uid = Some(document.uid);
        batch.document_revision = Some(document.revision);
        batch.total_pages = 6;
        batch.pages_per_batch = 3;
        batch.completed_pages = 3;
        let report = &mut batch.batches[0];
        report.page_count = 3;
        report.proof_paths.push("page-3.png".into());
        report.proof_sha256.push("4".repeat(64));
        fixture.app.ui.ai.accept(fixture.checkpoint.clone());
        fixture.app.ui.ai.settings.facing_pages = true;
        fixture.app.ui.ai.settings.right_to_left_binding = Some(false);
        let mut panel = fixture.panel();
        panel.run_steps(3);
        panel.get_by_label("Next").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.proof_binding, Some(true));
        assert!(panel.get_by_label("Page 3 proof").rect().center().x < panel.get_by_label("Page 2 proof").rect().center().x);
        panel.state_mut().run("layout.documentSetup", json!({"binding":"leftToRight"})).unwrap();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.proof_binding, None);
        assert!(!panel.state().ui.ai.can_review_selected_proof());
        assert!(!panel.state().ui.ai.can_approve_batch());
    }

    #[test]
    fn proof_navigation_rejects_foreign_pages_and_discards_a_press_on_the_previous_page() {
        let mut panel = ProofFixture::new().panel();
        panel.run_steps(3);
        assert!(!panel.state_mut().ui.ai.select_proof("foreign.png"));
        assert_eq!(panel.state().ui.ai.proof_loaded.as_deref(), Some("page-1.png"));
        let pos = panel.get_by_label("I reviewed the displayed page").rect().center();
        panel.event(egui::Event::PointerMoved(pos));
        panel.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: Default::default() });
        panel.run_steps(1);
        assert!(panel.state().ui.ai.review_pointer_press.is_some());
        assert!(panel.state_mut().ui.ai.select_proof("page-2.png"));
        assert!(panel.state().ui.ai.proof_texture.is_none());
        assert!(panel.state().ui.ai.review_pointer_press.is_none());
        panel.run_steps(2);
        panel.event(egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: Default::default() });
        panel.run_steps(3);
        assert!(panel.state().ui.ai.reviewed_proofs.is_empty(), "releasing a press from another page cannot confirm the new preview");
        panel.get_by_label("I reviewed the displayed page").click();
        panel.run_steps(3);
        assert_eq!(panel.state().ui.ai.reviewed_proofs, BTreeSet::from([("page-2.png".into(), "3".repeat(64))]));
    }

    #[test]
    fn batch_approval_requires_explicit_review_of_every_page_not_first_autoload() {
        let mut fixture = ProofFixture::new();
        load_proof_preview(&mut fixture.app, &fixture.ctx);
        assert!(fixture.app.ui.ai.proof_texture.is_some());
        assert!(fixture.app.ui.ai.reviewed_proofs.is_empty(), "autoload is not human review");
        assert!(!fixture.app.ui.ai.can_approve_batch());
        fixture.app.ui.ai.review_selected_proof();
        assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1);
        assert!(!fixture.app.ui.ai.can_approve_batch(), "first page alone must not approve a two-page batch");
        assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
        assert_eq!(fixture.calls.get(), 0);
        fixture.app.ui.ai.proof_path = Some("page-2.png".into());
        fixture.app.ui.ai.review_selected_proof();
        assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1, "the previous texture cannot confirm the newly selected page");
        fixture.view("page-2.png");
        assert!(!fixture.app.ui.ai.can_approve_batch(), "loading the second page is not confirmation");
        fixture.app.ui.ai.review_selected_proof();
        assert!(fixture.app.ui.ai.can_approve_batch());
    }

    #[test]
    fn batch_review_resets_for_checkpoint_proof_fingerprints_and_document_revision() {
        for change in ["checkpoint", "png hash", "pdf hash", "path", "page range", "task shape", "document uid", "document revision"] {
            let mut fixture = ProofFixture::new();
            fixture.confirm_all();
            assert!(fixture.app.ui.ai.can_approve_batch());
            let mut changed = fixture.checkpoint.clone();
            let batch = changed.batch.as_mut().unwrap();
            match change {
                "checkpoint" => batch.checkpoint += 1,
                "png hash" => batch.batches[0].proof_sha256[2] = "a".repeat(64),
                "pdf hash" => batch.batches[0].proof_sha256[1] = "a".repeat(64),
                "path" => batch.batches[0].proof_paths[2] = "replacement.png".into(),
                "page range" => batch.batches[0].page_start += 1,
                "task shape" => batch.total_pages += 1,
                "document uid" => batch.document_uid = batch.document_uid.map(|uid| uid + 1),
                "document revision" => batch.document_revision = batch.document_revision.map(|revision| revision + 1),
                _ => panic!("Unknown proof regression case: {change}"),
            }
            let document = Some((batch.document_uid.unwrap(), batch.document_revision.unwrap()));
            fixture.app.ui.ai.accept(changed);
            fixture.app.ui.ai.sync_proof_document(document);
            assert!(fixture.app.ui.ai.reviewed_proofs.is_empty(), "{change}");
            assert!(fixture.app.ui.ai.proof_texture.is_none(), "{change}");
            assert!(!fixture.app.ui.ai.can_approve_batch(), "{change}");
        }
        let mut fixture = ProofFixture::new();
        fixture.confirm_all();
        let document = fixture.app.ui.ai.proof_key.as_ref().unwrap().document;
        fixture.app.ui.ai.sync_proof_document(Some((document.0, document.1 + 1)));
        assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 2, "review evidence stays disabled until the host validates this revision");
        assert!(!fixture.app.ui.ai.can_review_selected_proof());
        assert!(!fixture.app.ui.ai.can_approve_batch());
    }

    #[test]
    fn empty_missing_duplicate_or_unfingerprinted_batch_pages_cannot_approve() {
        for change in ["empty", "missing png", "missing hash", "duplicate png", "duplicate pdf", "case alias png", "invalid hash"] {
            let mut fixture = ProofFixture::new();
            let mut changed = fixture.checkpoint.clone();
            let report = &mut changed.batch.as_mut().unwrap().batches[0];
            match change {
                "empty" => {
                    report.page_count = 0;
                    report.proof_paths.clear();
                    report.proof_sha256.clear();
                }
                "missing png" => {
                    report.proof_paths.pop();
                    report.proof_sha256.pop();
                }
                "missing hash" => {
                    report.proof_sha256.pop();
                }
                "duplicate png" => report.proof_paths[3] = report.proof_paths[2].clone(),
                "case alias png" => report.proof_paths[3] = "PAGE-1.PNG".into(),
                "duplicate pdf" => {
                    report.proof_paths.push("proof.pdf".into());
                    report.proof_sha256.push("1".repeat(64));
                }
                "invalid hash" => report.proof_sha256[2] = "missing".into(),
                _ => panic!("Unknown proof regression case: {change}"),
            }
            fixture.app.ui.ai.accept(changed);
            assert!(fixture.app.ui.ai.proof_key.is_none(), "{change}");
            assert!(!fixture.app.ui.ai.can_approve_batch(), "{change}");
        }
    }

    #[test]
    fn failed_page_reload_revokes_its_previous_human_confirmation() {
        let mut fixture = ProofFixture::new();
        fixture.confirm_all();
        assert!(fixture.app.ui.ai.can_approve_batch());
        fixture.app.services.read = Some(Box::new(|_| Err("synthetic proof read failure".into())));
        fixture.view("page-1.png");
        assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1);
        assert!(!fixture.app.ui.ai.can_approve_batch());
        fixture.app.ui.ai.review_selected_proof();
        assert_eq!(fixture.app.ui.ai.reviewed_proofs.len(), 1, "a failed preview cannot be confirmed");
    }

    #[test]
    fn control_cannot_approve_after_all_pages_reviewed_but_typed_human_action_can() {
        let mut fixture = ProofFixture::new();
        fixture.confirm_all();
        assert!(fixture.app.ui.ai.can_approve_batch());
        assert!(fixture.app.run("ai.request", json!({"method":"approve_batch", "job_id":"two-page-proof", "checkpoint":1})).is_err());
        assert_eq!(fixture.calls.get(), 0);
        assert!(
            dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err(),
            "absence of control input is not a human gesture"
        );
        let ctx = fixture.ctx.clone();
        let mut focused = false;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let response = ui.button("Approve proof");
            response.request_focus();
            focused = response.has_focus();
        });
        output.textures_delta.clear();
        assert!(focused);
        let input = egui::RawInput {
            events: vec![egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Default::default() }],
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, |ui| {
            let response = ui.button("Approve proof");
            assert!(AiUiState::fresh_review_gesture(
                true,
                fixture.app.ui.ai.proof_key.as_ref(),
                &mut fixture.app.ui.ai.review_pointer_press,
                &response
            ));
            fixture.app.ui.ai.batch_approval_gesture =
                fixture.app.ui.ai.proof_key.clone().map(|key| (key, ui.ctx().clone(), ui.ctx().cumulative_frame_nr()));
            dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).unwrap();
        });
        output.textures_delta.clear();
        assert_eq!(fixture.calls.get(), 1);
        assert!(fixture.app.ui.ai.reviewed_proofs.is_empty(), "the human approval is consumed once");
        assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
        assert_eq!(fixture.calls.get(), 1);
    }

    #[test]
    fn idle_batch_review_frames_never_confirm_or_approve_and_expired_gesture_is_rejected() {
        let mut fixture = ProofFixture::new();
        fixture.view("page-1.png");
        let ctx = fixture.ctx.clone();
        for reviewed in [false, true] {
            if reviewed {
                fixture.confirm_all();
            }
            let before = fixture.app.ui.ai.reviewed_proofs.clone();
            for _ in 0..4 {
                let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                    let mut requests = Vec::new();
                    batch_review(ui, &mut fixture.app.ui.ai, false, &mut requests);
                    assert!(requests.is_empty());
                    assert!(fixture.app.ui.ai.batch_approval_gesture.is_none());
                });
                output.textures_delta.clear();
                assert_eq!(fixture.app.ui.ai.reviewed_proofs, before);
            }
        }
        fixture.app.ui.ai.batch_approval_gesture = fixture.app.ui.ai.proof_key.clone().map(|key| (key, ctx.clone(), ctx.cumulative_frame_nr()));
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
        assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
        assert_eq!(fixture.calls.get(), 0);
    }

    #[test]
    fn review_gesture_rejects_repeated_keys_and_pointer_press_from_previous_proof() {
        fn frame(fixture: &mut ProofFixture, events: Vec<egui::Event>) -> (egui::Rect, bool) {
            let ctx = fixture.ctx.clone();
            let mut result = (egui::Rect::NOTHING, false);
            let mut output = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
                let response = ui.button("Confirm proof");
                response.request_focus();
                result = (
                    response.rect,
                    AiUiState::fresh_review_gesture(
                        true,
                        fixture.app.ui.ai.proof_key.as_ref(),
                        &mut fixture.app.ui.ai.review_pointer_press,
                        &response,
                    ),
                );
            });
            output.textures_delta.clear();
            result
        }
        let mut fixture = ProofFixture::new();
        let (rect, activated) = frame(&mut fixture, vec![]);
        assert!(!activated);
        let key = |repeat| egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: true, repeat, modifiers: Default::default() };
        assert!(frame(&mut fixture, vec![key(false)]).1);
        assert!(!frame(&mut fixture, vec![key(true)]).1);
        assert!(
            !frame(
                &mut fixture,
                vec![egui::Event::Key { key: egui::Key::Enter, physical_key: None, pressed: false, repeat: false, modifiers: Default::default() }]
            )
            .1
        );
        assert!(frame(&mut fixture, vec![key(false)]).1);
        let pointer =
            |pressed| egui::Event::PointerButton { pos: rect.center(), button: egui::PointerButton::Primary, pressed, modifiers: Default::default() };
        assert!(!frame(&mut fixture, vec![egui::Event::PointerMoved(rect.center()), pointer(true)]).1);
        assert!(fixture.app.ui.ai.review_pointer_press.is_some());
        let mut changed = fixture.checkpoint.clone();
        changed.batch.as_mut().unwrap().checkpoint += 1;
        fixture.app.ui.ai.accept(changed);
        assert!(!frame(&mut fixture, vec![pointer(false)]).1, "release cannot approve a replacement proof");
        assert!(!frame(&mut fixture, vec![pointer(true)]).1);
        assert!(frame(&mut fixture, vec![pointer(false)]).1, "a fresh press and release on the current proof remains usable");
        assert!(frame(&mut fixture, vec![pointer(true), pointer(false)]).1, "coalesced fresh pointer input remains usable");
        let outside = egui::Event::PointerButton {
            pos: rect.right_bottom() + egui::vec2(50.0, 50.0),
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        };
        assert!(!frame(&mut fixture, vec![outside, pointer(false)]).1, "a press originating outside the proof widget is not approval");
    }

    #[test]
    fn synthetic_control_frames_cannot_confirm_pages_or_submit_batch_approval() {
        let mut fixture = ProofFixture::new();
        fixture.view("page-1.png");
        fixture.app.control_input_frame.store(true, std::sync::atomic::Ordering::Relaxed);
        fixture.app.ui.ai.review_input_is_human = human_review_input(&fixture.app);
        fixture.app.ui.ai.review_selected_proof();
        assert!(fixture.app.ui.ai.reviewed_proofs.is_empty());
        fixture.app.control_input_frame.store(false, std::sync::atomic::Ordering::Relaxed);
        fixture.app.ui.ai.review_input_is_human = human_review_input(&fixture.app);
        fixture.confirm_all();
        for control_request in [true, false] {
            fixture.app.in_control = control_request;
            fixture.app.control_input_frame.store(!control_request, std::sync::atomic::Ordering::Relaxed);
            assert!(dispatch(&mut fixture.app, AiRequest::ApproveBatch { job_id: "two-page-proof".into(), checkpoint: 1 }).is_err());
            assert_eq!(fixture.calls.get(), 0);
        }
    }

    #[test]
    fn human_only_typed_dispatch_rejects_control_requests_and_synthetic_frames() {
        let mut fixture = ProofFixture::new();
        for control_request in [true, false] {
            fixture.app.in_control = control_request;
            fixture.app.control_input_frame.store(!control_request, std::sync::atomic::Ordering::Relaxed);
            for request in [
                AiRequest::ApproveSkill { id: "fixture".into(), revision: "1".into() },
                AiRequest::SubmitPreparedPrint { token: "fixture".into(), request_id: "fixture-op".into() },
                AiRequest::SaveApiKey { selection: fixture.app.ui.ai.settings.text_model.clone(), key: "fixture".into() },
                AiRequest::RemoveApiKey { selection: fixture.app.ui.ai.settings.text_model.clone() },
                AiRequest::RefreshCredentials,
                AiRequest::OpenPrintProof { token: "fixture".into() },
                AiRequest::StartLocalOllama { executable: "C:/fixture/ollama.exe".into() },
                AiRequest::StopLocalOllama,
            ] {
                assert!(dispatch(&mut fixture.app, request).is_err());
                assert_eq!(fixture.calls.get(), 0);
            }
        }
        fixture.app.in_control = false;
        fixture.app.control_input_frame.store(false, std::sync::atomic::Ordering::Relaxed);
        dispatch(&mut fixture.app, AiRequest::ApproveSkill { id: "fixture".into(), revision: "1".into() }).unwrap();
        dispatch(&mut fixture.app, AiRequest::SubmitPreparedPrint { token: "fixture".into(), request_id: "fixture-op".into() }).unwrap();
        assert_eq!(fixture.calls.get(), 2, "ordinary human typed actions still reach the mock host");
        assert!(is_non_text_model("gemini-3.1-flash-lite-image"));
        assert!(is_non_text_model("models/imagen-3.0-generate-002"));
        assert!(is_non_text_model("gemini-3.8-flash-tts"));
        assert!(is_non_text_model("dall-e-3"));
        assert!(is_non_text_model("gpt-image-1"));
        assert!(is_non_text_model("gemini-embedding-001"));
        assert!(!is_non_text_model("gemini-3.8-flash"));
        assert!(!is_non_text_model("gemini-flash-latest"));
        assert!(!is_non_text_model("gemini-pro-latest"));
        assert!(!is_non_text_model("silver-text"));
        assert!(!is_non_text_model("olive-chat:latest"));
        assert!(!is_non_text_model("imaginative-writer-7b"));
        let mut model_state = AiUiState { prompt: "Page 1 brief".into(), new_document: true, ..Default::default() };

        model_state.settings.output_mode = OutputMode::NativeOnly;
        model_state.settings.text_model.model = "gemini-3.1-flash-lite-image".into();
        assert!(model_state.production_issue(true, true, false).is_some_and(|msg| msg.contains("image/media model")));
        assert!(model_state.production_issue(true, true, true).is_some_and(|msg| msg.contains("للصور أو الوسائط")));
        model_state.settings.text_model.model = "gemini-3.8-flash".into();
        assert_eq!(model_state.production_issue(true, true, false), None);
    }

    #[test]
    fn live_status_page_map_and_script_conflict_actions_work_without_starting_production() {
        let mut live = AiUiState {
            snapshot: AiSnapshot {
                busy: true,
                stage: "planning".into(),
                message: "Generating editable native layout for page 23".into(),
                batch: Some(BatchSnapshot {
                    status: "running".into(),
                    current_batch: 1,
                    total_batches: 13,
                    completed_pages: 22,
                    total_pages: 48,
                    pages_per_batch: 2,
                    elapsed_ms: 5_000,
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(live.live_status_line(false), "Batch 1 / 13 · Page 23 / 48 · Phase: Planning · 00:00:05");
        assert_eq!(live.live_status_line(true), "الدفعة 1 / 13 · الصفحة 23 / 48 · المرحلة: التخطيط · 00:00:05");
        live.snapshot.stage = "rendering_proof".into();
        live.snapshot.message = "Preparing complete review proofs".into();
        live.snapshot.batch.as_mut().unwrap().elapsed_ms = 18_000;
        assert_eq!(live.live_status_line(false), "Batch 1 / 13 · Pages 23–24 / 48 · Phase: Proof preparation · 00:00:18");

        let mut state = AiUiState {
            new_document: false,
            continue_book: true,
            prompt: (1..=48).map(|page| format!("### PAGE {page:02} — Lesson {page}\n")).collect(),
            ..Default::default()
        };
        state.settings.text_model.model = "fixture-model".into();
        let documents = vec![
            OpenDocument {
                index: 0,
                uid: 10,
                path: None,
                title: "Untitled-11".into(),
                facing_pages: true,
                right_to_left_binding: false,
                pages: 49,
                remaining_pages: 49,
                continuable: false,
                filled_ranges: vec![[2, 15]],
                blank_ranges: vec![[1, 1], [16, 49]],
            },
            OpenDocument {
                index: 1,
                uid: 11,
                path: None,
                title: "book-48".into(),
                facing_pages: true,
                right_to_left_binding: false,
                pages: 48,
                remaining_pages: 26,
                continuable: true,
                filled_ranges: vec![[1, 22]],
                blank_ranges: vec![[23, 48]],
            },
        ];
        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1300.0, 1200.0)).build_ui_state(
            move |ui, (state, requests, selected)| production(ui, state, false, true, &documents, selected, requests),
            (state, Vec::new(), Some(0)),
        );
        panel.run_steps(3);
        assert!(panel.query_by_label("Page map — Completed: 2–15 · Blank: 1, 16–49").is_some());
        assert!(
            panel
                .query_by_label(
                    "Leading page 1 is blank while later pages contain items (2–15); prefix continuation requires completed pages starting at page 1."
                )
                .is_some()
        );
        assert!(panel.query_by_label("Script defines 48 explicit PAGE blocks, but selected document 'Untitled-11' has 49 pages.").is_some());
        assert!(panel.get_by_label("Produce").accesskit_node().is_disabled());

        panel.get_by_label("Switch to matching book (book-48)").click();
        panel.run_steps(3);
        assert_eq!(panel.state().2, Some(1));
        assert!(panel.state().1.is_empty(), "switching to the matching book never starts production automatically");
        assert!(panel.query_by_label("Page map — Completed: 1–22 · Blank: 23–48").is_some());
        assert!(!panel.get_by_label("Produce").accesskit_node().is_disabled());

        panel.state_mut().0.new_document = true;
        panel.state_mut().0.continue_book = false;
        panel.state_mut().0.pages = 8;
        panel.run_steps(3);
        assert!(panel.query_by_label("Script defines 48 explicit PAGE blocks, but New document is set to 8 pages.").is_some());
        assert!(panel.get_by_label("Produce").accesskit_node().is_disabled());
        panel.get_by_label("Match document page count to script (48 pages)").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.pages, 48);
        assert!(!panel.get_by_label("Produce").accesskit_node().is_disabled());
        assert!(panel.state().1.is_empty());
    }

    #[test]
    fn diagnostic_alert_banner_displays_cause_solution_and_executes_recovery_actions() {
        use egui_kittest::kittest::Queryable;

        let mut state = AiUiState {
            automatic_skills: false,
            selected_skills: std::collections::BTreeSet::from([
                "skill-1".to_string(),
                "skill-2".to_string(),
                "skill-3".to_string(),
                "skill-4".to_string(),
                "skill-5".to_string(),
            ]),
            ..Default::default()
        };
        state.snapshot.stage = "failed".into();
        state.snapshot.message = "AI context exceeds its byte limit".into();

        let alert = state.active_diagnostic_alert().expect("context limit alert should be active");
        assert_eq!(alert.kind, DiagnosticAlertKind::ContextLimit);

        let documents = vec![OpenDocument {
            index: 0,
            uid: 1,
            path: None,
            title: "KG2-Islamic".into(),
            facing_pages: true,
            right_to_left_binding: true,
            pages: 8,
            remaining_pages: 8,
            continuable: false,
            filled_ranges: Vec::new(),
            blank_ranges: vec![[1, 8]],
        }];

        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1300.0, 1200.0)).build_ui_state(
            move |ui, (state, requests, selected)| production(ui, state, false, true, &documents, selected, requests),
            (state, Vec::new(), Some(0)),
        );
        panel.run_steps(3);
        assert!(panel.query_by_label("⚠ AI Context Capacity Alert — Skill or Reference Context Limit Exceeded").is_some());
        panel.get_by_label("Enable Smart Skill Compaction").click();
        panel.run_steps(3);
        assert!(panel.state().0.automatic_skills);
        assert!(panel.state().0.selected_skills.len() <= 2);
        assert!(panel.state().0.active_diagnostic_alert().is_none());

        panel.state_mut().0.snapshot.stage = "failed".into();
        panel.state_mut().0.snapshot.message = "Provider request failed: 401 Unauthorized API key".into();
        panel.run_steps(3);
        assert!(panel.query_by_label("⚠ Authentication Alert — Provider API Key Missing or Unauthorized").is_some());
        panel.get_by_label("Open Keys & Providers").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.tab, 3);
    }

    #[test]
    fn production_panel_open_file_to_edit_switches_target_to_current_document() {
        use egui_kittest::kittest::Queryable;

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("existing-poster.designcraft");
        let txt_path = dir.path().join("imported-notes.txt");
        std::fs::write(&txt_path, "Imported heading line\nSecond body paragraph for editing.").unwrap();
        let mut author = DesignApp::new(
            Session::new(),
            Services { write: Some(Box::new(|path, bytes| std::fs::write(path, bytes).map_err(|e| e.to_string()))), ..Default::default() },
        );
        author.run("file.new", serde_json::json!({"preset":"A4","pages":2})).unwrap();
        author
            .run("frame.create", serde_json::json!({"spread":0,"rect":[72.0,72.0,320.0,160.0],"content":"text","text":"Existing Poster Heading"}))
            .unwrap();
        author.run("file.save", serde_json::json!({"path": file_path.to_string_lossy()})).unwrap();

        let observed_purpose = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let seen_purpose = observed_purpose.clone();
        let mut app = DesignApp::new(
            Session::new(),
            Services {
                read: Some(Box::new(|path| std::fs::read(path).map_err(|e| e.to_string()))),
                pick_open: Some(Box::new(move |purpose| {
                    *seen_purpose.borrow_mut() = purpose.to_string();
                    None
                })),
                ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })),
                ..Default::default()
            },
        );
        app.ui.ai.open = true;
        app.ui.ai.new_document = true;
        app.ui.ai.continue_book = true;
        app.ui.ai.open_document_path = file_path.to_string_lossy().into_owned();
        app.ui.ai.open_target_document_requested = true;

        let mut harness =
            egui_kittest::Harness::builder().with_size(egui::vec2(1300.0, 900.0)).build_ui_state(|ui, app: &mut DesignApp| show(app, ui.ctx()), app);
        harness.run_steps(3);

        assert!(!harness.state().ui.ai.new_document, "opening a file to edit switches target mode to CurrentDocument");
        assert!(!harness.state().ui.ai.continue_book, "opening a file to edit clears continue_book");
        assert!(harness.state().ui.ai.error.is_empty(), "error={}", harness.state().ui.ai.error);
        assert_eq!(harness.state().session.active().map(|d| d.doc.page_count()), Some(2));
        assert!(harness.query_by_label("Open path").is_some());
        assert!(harness.query_by_label("Browse…").is_some());

        // Cancelling the file picker must not flip target mode when a document is already open,
        // and must request the multi-format ai_target_document filter rather than generic open.
        harness.state_mut().ui.ai.new_document = true;
        harness.state_mut().ui.ai.pick_target_document = true;
        harness.run_steps(3);
        assert!(harness.state().ui.ai.new_document, "cancelled picker must preserve new_document mode");
        assert_eq!(observed_purpose.borrow().as_str(), "ai_target_document");

        assert_eq!(sanitize_pdf_page_size(Some((420.0, 595.0))), (420.0, 595.0));
        assert_eq!(sanitize_pdf_page_size(Some((f64::NAN, 595.0))), (595.27559, 841.88976));
        assert_eq!(sanitize_pdf_page_size(Some((-10.0, 595.0))), (595.27559, 841.88976));
        assert_eq!(sanitize_pdf_page_size(Some((10.0, 595.0))), (595.27559, 841.88976));
        assert_eq!(sanitize_pdf_page_size(Some((595.0, 99_999.0))), (595.27559, 841.88976));

        // Opening a text file imports and autoflows it into a new editable document and switches to CurrentDocument.
        harness.state_mut().ui.ai.continue_book = true;
        harness.state_mut().ui.ai.open_document_path = txt_path.to_string_lossy().into_owned();
        harness.state_mut().ui.ai.open_target_document_requested = true;
        harness.run_steps(3);
        assert!(!harness.state().ui.ai.new_document);
        assert!(!harness.state().ui.ai.continue_book);
        assert!(harness.state().ui.ai.error.is_empty(), "error={}", harness.state().ui.ai.error);
        assert_eq!(harness.state().session.active().map(|d| d.doc.title.as_str()), Some("imported-notes"));
    }

    #[test]
    fn selectable_skills_and_new_diagnostic_quick_fixes_operate_cleanly() {
        use egui_kittest::kittest::Queryable;

        let cards = vec![
            SkillCard {
                design_profile: None,
                id: "builtin-draft".into(),
                name: "Builtin Draft".into(),
                description: "Fresh unapproved builtin skill".into(),
                revision: "rev-1".into(),
                approved: false,
                automatically_validated: false,
                trusted_source: true,
                active: false,
                enabled: true,
                compatible: true,
            },
            SkillCard {
                design_profile: None,
                id: "untrusted-user-draft".into(),
                name: "Untrusted User Draft".into(),
                description: "Unapproved imported draft".into(),
                revision: "rev-ext".into(),
                approved: false,
                automatically_validated: false,
                trusted_source: false,
                active: false,
                enabled: true,
                compatible: true,
            },
            SkillCard {
                design_profile: None,
                id: "dual-rev".into(),
                name: "Dual Rev Active".into(),
                description: "Approved active revision".into(),
                revision: "rev-active".into(),
                approved: true,
                automatically_validated: true,
                trusted_source: false,
                active: true,
                enabled: true,
                compatible: true,
            },
            SkillCard {
                design_profile: None,
                id: "dual-rev".into(),
                name: "Dual Rev Newer Draft".into(),
                description: "Unapproved newer draft".into(),
                revision: "rev-draft".into(),
                approved: false,
                automatically_validated: false,
                trusted_source: false,
                active: false,
                enabled: true,
                compatible: true,
            },
            SkillCard {
                design_profile: None,
                id: "disabled-skill".into(),
                name: "Disabled".into(),
                description: "Disabled".into(),
                revision: "rev-dis".into(),
                approved: true,
                automatically_validated: false,
                trusted_source: true,
                active: true,
                enabled: false,
                compatible: true,
            },
        ];
        let manual_visible = selectable_skill_cards(&cards, false, true);
        assert_eq!(manual_visible.len(), 2, "manual mode must hide unapproved untrusted drafts");
        assert_eq!(manual_visible[0].id, "builtin-draft");
        assert_eq!(manual_visible[1].id, "dual-rev");
        assert_eq!(manual_visible[1].revision, "rev-active", "active revision must win over newer draft without duplicating IDs");

        let auto_pin_visible = selectable_skill_cards(&cards, false, false);
        assert_eq!(auto_pin_visible.len(), 3, "automatic mode pin list allows automatic validation candidates");
        assert_eq!(auto_pin_visible[1].id, "untrusted-user-draft");

        let mut state = AiUiState {
            prompt: "# PAGE 1 — Cover\nTITLE_AR: أ\n# PAGE 2 — Intro\nTITLE_AR: ب\n# PAGE 3 — Lesson\nTITLE_AR: ج\n# PAGE 4 — Review\nTITLE_AR: د\n"
                .into(),
            new_document: true,
            pages: 12,
            ..Default::default()
        };
        state.settings.allow_paid_repairs = false;
        state.settings.budget.repair_attempts = 0;
        state.snapshot.stage = "failed".into();
        state.snapshot.message = "Script defines 4 explicit PAGE blocks, but target page count differs".into();

        let documents = Vec::new();
        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1300.0, 1200.0)).build_ui_state(
            move |ui, (state, requests, selected)| production(ui, state, false, true, &documents, selected, requests),
            (state, Vec::new(), None),
        );
        panel.run_steps(3);
        panel.get_by_label("Match Page Count to Script").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.pages, 4);
        assert!(panel.state().0.active_diagnostic_alert().is_none());

        panel.state_mut().0.snapshot.stage = "failed".into();
        panel.state_mut().0.snapshot.message = "Print-preflight errors: text frame overflow".into();
        panel.run_steps(3);
        panel.get_by_label("Enable Automatic Layout Repair").click();
        panel.run_steps(3);
        assert!(panel.state().0.settings.allow_paid_repairs);
        assert!(panel.state().0.settings.budget.repair_attempts >= 2);
        assert!(panel.state().0.active_diagnostic_alert().is_none());
    }

    #[test]
    fn model_validation_retired_404_quick_fixes_and_per_provider_discovery_operate_cleanly() {
        use egui_kittest::kittest::Queryable;

        // 1. Pre-flight production_issue checks for image_model and verify_generated_images (English + Arabic).
        let mut state = AiUiState { prompt: "Illustrated worksheet".into(), new_document: true, pages: 4, ..Default::default() };
        state.settings.output_mode = OutputMode::NativeWithImages;
        state.settings.text_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None };
        state.settings.image_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None };
        assert!(state.production_issue(true, true, false).unwrap().contains("text/non-image"));
        assert!(state.production_issue(true, true, true).unwrap().contains("مخصص للنصوص"));

        state.settings.image_model = ModelSelection { provider: ProviderKind::Ollama, model: "llava:latest".into(), endpoint: None };
        assert!(state.production_issue(true, true, false).unwrap().contains("Ollama"));

        state.settings.image_model =
            ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.1-flash-image-preview".into(), endpoint: None };
        state.settings.verify_generated_images = true;
        state.settings.text_model = ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-5.4".into(), endpoint: None };
        assert!(state.production_issue(true, true, false).unwrap().contains("Gemini"));

        // 2. HTTP 404 / retired gemini-2.5-flash diagnostic alert banner & 1-click quick fix persisting UpdateSettings.
        state.settings.verify_generated_images = false;
        state.settings.text_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None };
        state.snapshot.stage = "failed".into();
        state.snapshot.message = "AI provider HTTP 404: request, model, schema, or endpoint not supported".into();
        assert_eq!(state.active_diagnostic_alert().map(|alert| alert.kind), Some(DiagnosticAlertKind::InvalidModel));

        let documents = Vec::new();
        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1300.0, 1200.0)).build_ui_state(
            move |ui, (state, requests, selected)| production(ui, state, false, true, &documents, selected, requests),
            (state, Vec::new(), None),
        );
        panel.run_steps(3);
        panel.get_by_label("Use gemini-3.8-flash").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.text_model.model, "gemini-3.8-flash");
        assert!(panel.state().1.iter().any(|req| matches!(req, AiRequest::UpdateSettings(s) if s.text_model.model == "gemini-3.8-flash")));
        assert!(panel.state().0.active_diagnostic_alert().is_none());

        // 3. Per-provider Discover populates discovered_models without clobbering across Gemini and OpenAI.
        let mut discover_state = AiUiState {
            settings: AiSettings {
                text_model: ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None },
                image_model: ModelSelection { provider: ProviderKind::OpenAi, model: "gpt-image-1".into(), endpoint: None },
                ..Default::default()
            },
            pending_models: Some(ModelSelection { provider: ProviderKind::Gemini, model: String::new(), endpoint: None }),
            ..Default::default()
        };
        let mut gemini_snap = discover_state.snapshot.clone();
        gemini_snap.busy = false;
        gemini_snap.stage = "completed".into();
        gemini_snap.models = vec!["gemini-3.8-flash".into(), "gemini-3.1-flash-image-preview".into(), "text-embedding-004".into()];
        discover_state.accept(gemini_snap);

        discover_state.pending_models = Some(ModelSelection { provider: ProviderKind::OpenAi, model: String::new(), endpoint: None });
        let mut openai_snap = discover_state.snapshot.clone();
        openai_snap.busy = false;
        openai_snap.stage = "completed".into();
        openai_snap.models = vec!["gpt-5.4".into(), "gpt-image-1".into(), "dall-e-3".into()];
        discover_state.accept(openai_snap);

        let gemini_models = discover_state.discovered_models.get(&(ProviderKind::Gemini, None)).unwrap();
        let openai_models = discover_state.discovered_models.get(&(ProviderKind::OpenAi, None)).unwrap();
        assert_eq!(gemini_models.len(), 3);
        assert_eq!(openai_models.len(), 3);
        assert_eq!(gemini_models.iter().filter(|m| !is_non_text_model(m)).cloned().collect::<Vec<_>>(), vec!["gemini-3.8-flash".to_owned()]);
        assert_eq!(
            openai_models.iter().filter(|m| !designcraft_ai_types::is_non_image_model(ProviderKind::OpenAi, m)).cloned().collect::<Vec<_>>(),
            vec!["gpt-image-1".to_owned(), "dall-e-3".to_owned()]
        );

        // 4. Gemini imagen-* (:predict-only) is rejected on :generateContent with gemini-3.1-flash-image guidance,
        // and Run [100] (text=gemini-3.8-flash, image=gemini-2.5-flash) renders the 1-click "Use gemini-3.1-flash-image" button.
        let mut img_state = AiUiState { prompt: "Illustrated page".into(), new_document: true, pages: 4, ..Default::default() };
        img_state.settings.output_mode = OutputMode::NativeWithImages;
        img_state.settings.text_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-3.8-flash".into(), endpoint: None };
        img_state.settings.image_model = ModelSelection { provider: ProviderKind::Gemini, model: "imagen-3.0-generate-002".into(), endpoint: None };
        let issue_en = img_state.production_issue(true, true, false).unwrap();
        assert!(issue_en.contains("gemini-3.1-flash-image") && !issue_en.contains("imagen-3.0-generate-002"), "{issue_en}");

        img_state.settings.image_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None };
        img_state.snapshot.stage = "failed".into();
        img_state.snapshot.message = "AI provider HTTP 404: request, model, schema, or endpoint not supported".into();
        let docs = Vec::new();
        let mut img_panel = egui_kittest::Harness::builder().with_size(egui::vec2(1300.0, 1200.0)).build_ui_state(
            move |ui, (state, requests, selected)| production(ui, state, false, true, &docs, selected, requests),
            (img_state, Vec::new(), None),
        );
        img_panel.run_steps(3);
        img_panel.get_by_label("Use gemini-3.1-flash-image").click();
        img_panel.run_steps(3);
        assert_eq!(img_panel.state().0.settings.image_model.model, "gemini-3.1-flash-image");
        assert!(img_panel.state().1.iter().any(|req| matches!(req, AiRequest::UpdateSettings(s) if s.image_model.model == "gemini-3.1-flash-image")));
        assert!(img_panel.state().0.active_diagnostic_alert().is_none());

        // 5. Opened facing-pages RTL book enables facing proof pair view even when AiUiState.settings.facing_pages is false.
        let batch = BatchSnapshot {
            job_id: "opened-rtl-book".into(),
            status: "awaiting_human_proof".into(),
            document_uid: Some(77),
            document_revision: Some(12),
            total_pages: 48,
            completed_pages: 2,
            pages_per_batch: 2,
            current_batch: 1,
            total_batches: 24,
            batches: vec![designcraft_ai_types::protocol::BatchReport {
                batch: 1,
                page_start: 1,
                page_count: 2,
                proof_paths: vec!["proof-p2.png".into(), "proof-p3.png".into()],
                proof_sha256: vec!["1".repeat(64), "2".repeat(64)],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut opened_book_ui = AiUiState::default();
        opened_book_ui.settings.facing_pages = false;
        opened_book_ui.accept(AiSnapshot { available: true, stage: "awaiting_human_proof".into(), batch: Some(batch), ..Default::default() });
        opened_book_ui.sync_proof_document(Some((77, 12)));
        opened_book_ui.proof_facing_pages = Some(true);
        opened_book_ui.proof_binding = Some(true);
        assert!(opened_book_ui.effective_proof_facing_pages());
        assert_eq!(opened_book_ui.companion_proof_path(), Some("proof-p3.png"));

        // 6. Reopened failed batch checkpoint surfaces InvalidModel from reports, and benign settings-saved confirmation never triggers a fake alert.
        let mut reopened_failed = AiUiState::default();
        reopened_failed.snapshot.batch = Some(BatchSnapshot { status: "failed".into(), ..Default::default() });
        reopened_failed.snapshot.reports.push(designcraft_ai_types::protocol::TaskReport {
            design_style: None,
            id: "100".into(),
            status: "failed".into(),
            title: "Batch 1".into(),
            provider: ProviderKind::Gemini,
            model: "gemini-2.5-flash".into(),
            settings: None,
            skills: Vec::new(),
            usage: Default::default(),
            elapsed_ms: 480,
            actions: 0,
            issues: Vec::new(),
            artifacts: Vec::new(),
            message: "AI provider HTTP 404: request, model, schema, or endpoint not supported".into(),
        });
        assert_eq!(reopened_failed.active_diagnostic_alert().unwrap().kind, DiagnosticAlertKind::InvalidModel);
        reopened_failed.snapshot.stage = "idle".into();
        reopened_failed.snapshot.message = "AI settings saved; API keys remain outside settings and reports".into();
        assert!(reopened_failed.active_diagnostic_alert().is_none());
    }

    #[test]
    fn ai27_preflight_natural_arabic_pages_retired_gemini_token_limits_and_persisted_allowances() {
        use egui_kittest::kittest::Queryable;

        // 1. Pure natural_prompt_pages parser handles Arabic-Indic digits, headers, English "-page"/"pages",
        //    and ignores page references ("انظر صفحة 5", "see page 2") and per-batch counts ("4 pages per review").
        assert_eq!(natural_prompt_pages("تصميم كتاب من ٤٨ صفحة رياض أطفال"), Some(48));
        assert_eq!(natural_prompt_pages("عدد الصفحات: ٢٤"), Some(24));
        assert_eq!(natural_prompt_pages("Design a 32-page workbook"), Some(32));
        assert_eq!(natural_prompt_pages("Create an illustrated guide of 16 pages."), Some(16));
        assert_eq!(natural_prompt_pages("انظر صفحة 5 وصفحة 6 لمزيد من التفاصيل"), None);
        assert_eq!(natural_prompt_pages("See page 2 and 4 pages per review"), None);
        assert_eq!(natural_prompt_pages("```\n48 pages\n```\nانظر صفحة 3"), None);

        // 2. Natural Arabic prompt with s.pages = 8 shows mismatch button & updates s.pages = 48,
        //    proactive retired gemini-2.5-flash warning switches to gemini-3.8-flash and emits UpdateSettings,
        //    proactive token limit button updates ONLY token_limit + emits UpdateSettings without mutating max_requests,
        //    and "Set request allowance for this review" persists UpdateSettings without mutating token_limit.
        let mut state = AiUiState {
            prompt: "تصميم كتاب من ٤٨ صفحة رياض أطفال".into(), new_document: true, pages: 8, ..Default::default()
        };
        state.settings.text_model = ModelSelection { provider: ProviderKind::Gemini, model: "gemini-2.5-flash".into(), endpoint: None };
        state.settings.pages_per_batch = 6;
        state.settings.budget.token_limit = 32_000;
        assert_eq!(state.settings.budget.max_requests, 4);

        let mut panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1300.0, 1400.0))
            .build_ui_state(|ui, (state, requests)| production(ui, state, false, true, &[], &mut None, requests), (state, Vec::new()));
        panel.run_steps(3);

        // Match document pages to prompt (48 pages)
        panel.get_by_label("Match document pages to prompt (48 pages)").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.pages, 48);
        assert!(panel.query_by_label("Match document pages to prompt (48 pages)").is_none());

        // Proactive retired Gemini model switch
        panel.get_by_label("Use gemini-3.8-flash").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.text_model.model, "gemini-3.8-flash");
        assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.text_model.model == "gemini-3.8-flash"));
        assert!(panel.query_by_label("Use gemini-3.8-flash").is_none());

        // Proactive token limit button updates ONLY token_limit (not max_requests!) and emits UpdateSettings
        panel.state_mut().1.clear();
        panel.get_by_label("Set token limit for this review").click();
        panel.run_steps(3);
        let raised_review_tokens = panel.state().0.settings.budget.token_limit;
        assert!(raised_review_tokens > 32_000);
        assert_eq!(panel.state().0.settings.budget.max_requests, 4, "token limit button must never mutate max_requests");
        assert!(matches!(
            panel.state().1.last(),
            Some(AiRequest::UpdateSettings(s)) if s.budget.token_limit == raised_review_tokens && s.budget.max_requests == 4
        ));

        // Request allowance button persists UpdateSettings without mutating token_limit
        panel.state_mut().1.clear();
        panel.get_by_label("Set request allowance for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.max_requests, 6);
        assert_eq!(panel.state().0.settings.budget.token_limit, raised_review_tokens, "request allowance button must leave token_limit untouched");
        assert!(matches!(
            panel.state().1.last(),
            Some(AiRequest::UpdateSettings(s)) if s.budget.max_requests == 6 && s.budget.token_limit == raised_review_tokens
        ));

        // Recovery UX when completed_pages == 0 offers accurate guidance + Start fresh with current settings
        panel.state_mut().1.clear();
        panel.state_mut().0.snapshot.production_draft = Some(designcraft_ai_types::protocol::ProductionDraft {
            id: "failed-zero".into(),
            request: panel.state().0.production_task(),
            document_uid: None,
            document_path: None,
            document_title: "Untitled".into(),
            progress: Default::default(),
        });
        panel.state_mut().0.snapshot.batch = Some(BatchSnapshot {
            job_id: "failed-zero".into(),
            status: "failed".into(),
            retryable: true,
            completed_pages: 0,
            total_pages: 8,
            pages_per_batch: 2,
            ..Default::default()
        });
        panel.run_steps(3);
        assert!(panel.query_by_label("Adjust limits or settings below, then Retry this group or start fresh.").is_some());
        assert!(
            panel.query_by_label("Open and select the saved book with remaining blank pages.").is_none(),
            "unpublished NewA4 failure with no document_uid/document_path must not prompt to open a non-existent saved book"
        );
        panel.get_by_label("Start fresh with current settings").click();
        panel.run_steps(3);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::Start(TaskRequest { target: TaskTarget::NewA4 { pages: 48 }, .. }))));
    }

    #[test]
    fn ai27_round1_audit_auto_style_estimates_vision_tokens_recovery_copy_and_natural_pages_edges() {
        use egui_kittest::kittest::Queryable;

        // 1. Fix 5: natural_prompt_pages edge cases (harakat/tatweel, attached units, brackets, singular "page", and negative rejections).
        assert_eq!(natural_prompt_pages("٤٨صفحة"), Some(48));
        assert_eq!(natural_prompt_pages("48صفحة"), Some(48));
        assert_eq!(natural_prompt_pages("48pages"), Some(48));
        assert_eq!(natural_prompt_pages("عدد الصفحات: (٤٨)"), Some(48));
        assert_eq!(natural_prompt_pages("عَدَدُ الصَّفَحَاتِ: [٤٨]"), Some(48));
        assert_eq!(natural_prompt_pages("كتاب من (48) صَفْحَة"), Some(48));
        assert_eq!(natural_prompt_pages("كتاب من ٤٨ صَفْحَـــةً رياض أطفال"), Some(48));
        assert_eq!(natural_prompt_pages("create a 48 page workbook"), Some(48));

        assert_eq!(natural_prompt_pages("page 48"), None);
        assert_eq!(natural_prompt_pages("انظر صفحة 5"), None);
        assert_eq!(natural_prompt_pages("انظر صَفْحَة (5)"), None);
        assert_eq!(natural_prompt_pages("48 pages per review"), None);
        assert_eq!(natural_prompt_pages("v2.48"), None);
        assert_eq!(natural_prompt_pages("v2.48 pages"), None);

        // 2. Fix 1a: Automatic-style KG illustrated prompt with `kg-oxford-cambridge-style` in `snapshot.skills`
        //    estimates `sources_per_page == 2`, manual `selected_skills` resolves profile, and 0-slot profile clamps to >= 1.
        let kg_manifest: Value = serde_json::from_str(include_str!("../../../skills/kg-oxford-cambridge-style/designcraft.json")).unwrap();
        let kg_profile: designcraft_ai_types::DesignProfile = serde_json::from_value(kg_manifest["design_profile"].clone()).unwrap();
        let kg_card = SkillCard {
            design_profile: Some(kg_profile.clone()),
            id: "kg-oxford-cambridge-style".into(),
            name: kg_profile.name.clone(),
            description: "International KG style".into(),
            revision: "rev-1".into(),
            approved: true,
            automatically_validated: true,
            trusted_source: true,
            active: true,
            enabled: true,
            compatible: true,
        };

        let mut kg_state = AiUiState {
            prompt: "تصميم كتاب منهج رياض أطفال دولي مصور من 8 صفحات".into(),
            new_document: true,
            pages: 8,
            automatic_skills: true,
            ..Default::default()
        };
        kg_state.settings.output_mode = OutputMode::NativeWithImages;
        kg_state.settings.pages_per_batch = 2;
        kg_state.settings.design_style = None; // "Automatic from the request"
        kg_state.snapshot.skills = vec![kg_card.clone()];

        let auto_est = kg_state.request_estimate(&[], None).unwrap();
        assert_eq!(auto_est.review_pages, 2);
        assert_eq!(auto_est.image_allowance, 4, "automatic KG Oxford/Cambridge style must estimate 2 image sources per page");

        // Manual skill selection (`automatic_skills == false`) also resolves selected style card
        kg_state.automatic_skills = false;
        kg_state.selected_skills.insert("kg-oxford-cambridge-style".into());
        kg_state.prompt = "Generic brief without style keywords".into();
        let manual_est = kg_state.request_estimate(&[], None).unwrap();
        assert_eq!(manual_est.image_allowance, 4);

        // Pinned active style in `selected_skills` with `automatic_skills == true` and generic prompt resolves profile
        kg_state.automatic_skills = true;
        kg_state.pages = 4;
        kg_state.prompt = "Create a 4-page activity book".into();
        let pinned_auto_est = kg_state.request_estimate(&[], None).unwrap();
        assert_eq!(pinned_auto_est.review_pages, 2);
        assert_eq!(
            pinned_auto_est.image_allowance, 4,
            "automatic_skills == true with active style in selected_skills must resolve that pinned style even on a generic prompt"
        );
        kg_state.pages = 8;
        kg_state.automatic_skills = false;

        // Zero-image-slot profile under NativeWithImages clamps sources_per_page to >= 1 (never 0)
        let mut zero_slot_profile = kg_profile;
        for family in &mut zero_slot_profile.page_families {
            family.image_slots.clear();
        }
        let mut zero_slot_card = kg_card;
        zero_slot_card.design_profile = Some(zero_slot_profile);
        kg_state.snapshot.skills = vec![zero_slot_card];
        let clamped_est = kg_state.request_estimate(&[], None).unwrap();
        assert_eq!(clamped_est.image_allowance, 2, "0-slot profile in NativeWithImages must clamp sources_per_page to 1");

        // 3. Fix 1b: Single-page review with `verify_generated_images` + skill overhead NEVER under-estimates `reserve_with_output`.
        let mut single_generic = AiUiState {
            prompt: "Create an illustrated science worksheet.".into(),
            new_document: true,
            pages: 1,
            automatic_skills: true,
            ..Default::default()
        };
        single_generic.settings.output_mode = OutputMode::NativeWithImages;
        single_generic.settings.verify_generated_images = true;
        single_generic.settings.pages_per_batch = 1;
        single_generic.settings.budget.max_output_tokens = 8192;
        let tok_est = single_generic.token_estimate(&[], None).unwrap();
        // Exact worst-case ledger `reserve_with_output` sum for 1-page generic multi-skill + 1 image + 1 vision review:
        // text call (up to 176 KiB context + prompt): (180_224 + prompt.len()).div_ceil(3) + 1024 + 8192
        // image call (~6 KiB prompt/style, image_call=true -> 0 attachment/output tokens): 2000 + 1024 = 3_024
        // vision review call (~6 KiB prompt/schema, 1 image, max_output=4096): 2000 + 1024 + 8192 + 4096 = 15_312
        let text_reserved = (176 * 1024u64 + single_generic.prompt.len() as u64).div_ceil(3) + 1024 + 8192;
        let image_reserved = 2000u64 + 1024;
        let review_reserved = 2000u64 + 1024 + 8192 + 4096;
        assert!(
            tok_est.review_tokens >= text_reserved + image_reserved + review_reserved,
            "review_tokens ({}) must never be smaller than actual reserve_with_output ({})",
            tok_est.review_tokens,
            text_reserved + image_reserved + review_reserved
        );

        // 4. Fix 4: Bound failed batch with `completed_pages > 0` shows saved book instructions + accurate Retry/Continue copy (EN + AR).
        let mut session = Session::new();
        session.execute("file.new", &json!({"pages": 4, "facingPages": false})).unwrap();
        session.execute("frame.create", &json!({"spread": 0, "rect": [20, 20, 100, 80]})).unwrap();
        let doc = session.active().unwrap();
        let req = continuation_request(TaskTarget::ContinueBook);
        let mut snap = saved_production_snapshot(&req, doc);
        snap.batch = Some(BatchSnapshot {
            job_id: "saved-book".into(),
            status: "failed".into(),
            retryable: true,
            completed_pages: 1,
            total_pages: 4,
            pages_per_batch: 1,
            document_uid: Some(doc.uid),
            document_revision: Some(doc.revision),
            ..Default::default()
        });
        let mut rec_state = AiUiState::default();
        rec_state.accept(snap);
        let docs = vec![OpenDocument::from_document(0, doc)];
        let mut en_panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1300.0, 1000.0))
            .build_ui_state(|ui, (s, reqs)| production(ui, s, false, true, &docs, &mut Some(0), reqs), (rec_state.clone(), Vec::new()));
        en_panel.run_steps(3);
        assert!(en_panel.query_by_label("Prepare saved book continuation").is_some());
        assert!(
            en_panel
                .query_by_label(
                    "Retry keeps cumulative usage, pinned style, and pagination while applying updated budget limits and compatible model selections. To change the brief, style, or batch size, prepare Continue and press Produce."
                )
                .is_some()
        );

        let docs_ar = vec![OpenDocument::from_document(0, doc)];
        let mut ar_fonts_ready = false;
        let mut ar_panel = egui_kittest::Harness::builder().with_size(egui::vec2(1300.0, 1000.0)).build_ui_state(
            move |ui, (s, reqs)| {
                if !ar_fonts_ready {
                    crate::theme::install_fonts(ui.ctx(), "ar");
                    ar_fonts_ready = true;
                    return;
                }
                production(ui, s, true, true, &docs_ar, &mut Some(0), reqs);
            },
            (rec_state, Vec::new()),
        );
        ar_panel.run_steps(3);
        assert!(ar_panel.query_by_label("تجهيز استكمال الكتاب المحفوظ").is_some());
        assert!(
            ar_panel
                .query_by_label(
                    "تحتفظ إعادة المحاولة بالاستخدام التراكمي والنمط المثبّت وتقسيم الدفعات مع تطبيق حدود الميزانية والنماذج المتوافقة المحدّثة. لتغيير التعليمات أو النمط أو حجم الدفعة، جهّز الاستكمال واضغط إنتاج."
                )
                .is_some()
        );

        // 5. Unpublished `NewA4` draft (`document_uid: None, document_path: None`) omits `"Prepare saved book continuation"`,
        //    whereas restored unsaved `ContinueBook` draft (`document_uid: None, document_path: None`, `restored_target_unconfirmed = true`)
        //    renders `"Prepare saved book continuation"` and enables `"Produce"` once clicked on a continuable open document.
        let mut unpub_snap = saved_production_snapshot(&continuation_request(TaskTarget::NewA4 { pages: 8 }), doc);
        if let Some(d) = &mut unpub_snap.production_draft {
            d.document_uid = None;
            d.document_path = None;
        }
        let mut unpub_state = AiUiState::default();
        unpub_state.accept(unpub_snap);
        let docs_restored = vec![OpenDocument::from_document(0, doc)];
        let mut unpub_panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1300.0, 1000.0))
            .build_ui_state(|ui, (s, reqs)| production(ui, s, false, true, &docs_restored, &mut Some(0), reqs), (unpub_state, Vec::new()));
        unpub_panel.run_steps(3);
        assert!(
            unpub_panel.query_by_label("Prepare saved book continuation").is_none(),
            "unpublished NewA4 draft with no document_uid/document_path must omit Prepare saved book continuation"
        );

        let mut restored_cont_snap = saved_production_snapshot(&continuation_request(TaskTarget::ContinueBook), doc);
        if let Some(d) = &mut restored_cont_snap.production_draft {
            d.document_uid = None;
            d.document_path = None;
        }
        let mut restored_cont_state = AiUiState::default();
        restored_cont_state.accept(restored_cont_snap);
        assert!(restored_cont_state.restored_target_unconfirmed);
        let docs_cont = vec![OpenDocument::from_document(0, doc)];
        let mut cont_panel = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1300.0, 1000.0))
            .build_ui_state(|ui, (s, reqs)| production(ui, s, false, true, &docs_cont, &mut Some(0), reqs), (restored_cont_state, Vec::new()));
        cont_panel.run_steps(3);
        assert!(cont_panel.query_by_label("Choose the target book or prepare its remaining pages before producing the saved request.").is_some());
        cont_panel.get_by_label("Prepare saved book continuation").click();
        cont_panel.run_steps(3);
        assert!(!cont_panel.state().0.restored_target_unconfirmed);
        cont_panel.get_by_label("Produce").click();
        cont_panel.run_steps(3);
        assert!(matches!(cont_panel.state().1.last(), Some(AiRequest::Start(TaskRequest { target: TaskTarget::ContinueBook, .. }))));
    }

    #[test]
    fn retryable_failed_batch_allowance_buttons_include_consumed_requests_and_tokens() {
        use designcraft_ai_types::protocol::UsageSummary;
        use egui_kittest::kittest::Queryable;

        let mut session = Session::new();
        session.execute("file.new", &json!({"pages": 20, "facingPages": false})).unwrap();
        for spread in 0..8 {
            session.execute("frame.create", &json!({"spread": spread, "rect": [20, 20, 100, 80]})).unwrap();
        }
        let doc = session.active().unwrap();
        let open_doc = OpenDocument::from_document(0, doc);
        assert_eq!(open_doc.pages, 20);
        assert_eq!(open_doc.remaining_pages, 12);
        assert!(open_doc.continuable);

        let mut req = continuation_request(TaskTarget::ContinueBook);
        req.settings.pages_per_batch = 2;
        req.settings.budget.max_requests = 8;
        req.settings.budget.token_limit = 120_000;

        let mut snap = saved_production_snapshot(&req, doc);
        snap.batch = Some(BatchSnapshot {
            job_id: "saved-book".into(),
            status: "failed".into(),
            retryable: true,
            completed_pages: 8,
            total_pages: 20,
            pages_per_batch: 2,
            document_uid: Some(doc.uid),
            document_revision: Some(doc.revision),
            cumulative_usage: UsageSummary { requests: 8, input_tokens: 100_000, output_tokens: 20_000, ..Default::default() },
            ..Default::default()
        });

        let mut state = AiUiState::default();
        state.accept(snap);
        state.restored_target_unconfirmed = false;
        assert!(!state.new_document);
        assert!(state.continue_book);

        let docs = vec![open_doc];
        let fresh_tokens = state.token_estimate(&docs, Some(0)).unwrap();
        let expected_review_tokens = 120_000u64.saturating_add(fresh_tokens.review_tokens).clamp(256, MAX_TOKEN_LIMIT);
        let expected_book_tokens = 120_000u64.saturating_add(fresh_tokens.book_tokens).clamp(256, MAX_TOKEN_LIMIT);

        let mut panel = egui_kittest::Harness::builder().with_size(egui::vec2(1300.0, 1400.0)).build_ui_state(
            |ui, (s, reqs, selected, docs)| production(ui, s, false, true, docs, selected, reqs),
            (state, Vec::new(), Some(0usize), docs),
        );
        panel.run_steps(3);

        // 1. Review request allowance includes 8 consumed requests + 2 review requests = 10.
        assert!(
            panel
                .query_by_label(
                    "Configured request limit 8 is below the conservative cumulative allowance to retry this review: 10 (8 already used)."
                )
                .is_some()
        );
        panel.get_by_label("Set request allowance for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.max_requests, 10);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.max_requests == 10));

        // 2. Remaining book request allowance includes 8 consumed requests + 12 remaining requests = 20.
        panel.state_mut().1.clear();
        panel.get_by_label("Set request allowance for remaining book").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.max_requests, 20);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.max_requests == 20));
        assert!(panel.query_by_label("Set request allowance for remaining book").is_none());

        // 3. Review and remaining-book token limits include the 120,000 consumed tokens on top of remaining estimates.
        panel.state_mut().1.clear();
        panel.get_by_label("Set token limit for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.token_limit, expected_review_tokens);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.token_limit == expected_review_tokens));

        panel.state_mut().1.clear();
        panel.get_by_label("Set token limit for remaining book").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.token_limit, expected_book_tokens);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.token_limit == expected_book_tokens));

        // Diagnostic alert quick fixes ("Raise Request Allowance" and "Raise Token Limit") also include consumed + estimated_tokens.
        panel.state_mut().1.clear();
        panel.state_mut().0.settings.budget.max_requests = 8;
        panel.state_mut().0.snapshot.batch.as_mut().unwrap().cumulative_usage.requests = 12;
        panel.state_mut().0.snapshot.stage = "failed".into();
        panel.state_mut().0.snapshot.message = "AI request limit reached".into();
        panel.run_steps(3);
        panel.get_by_label("Raise Request Allowance").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.max_requests, 24, "12 consumed + 12 remaining = 24 (> 8 * 2)");
        assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.max_requests == 24));

        panel.state_mut().1.clear();
        panel.state_mut().0.settings.budget.token_limit = 100_000;
        panel.state_mut().0.snapshot.batch.as_mut().unwrap().cumulative_usage.estimated_tokens = 90_000;
        panel.state_mut().0.snapshot.stage = "failed".into();
        panel.state_mut().0.snapshot.message = "Token budget cannot admit another request".into();
        panel.run_steps(3);
        panel.get_by_label("Raise Token Limit").click();
        panel.run_steps(3);
        let expected_raised_tokens = 210_000u64.saturating_add(fresh_tokens.book_tokens).clamp(256_000, MAX_TOKEN_LIMIT);
        assert_eq!(panel.state().0.settings.budget.token_limit, expected_raised_tokens);
        assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.token_limit == expected_raised_tokens));

        // Mid-book NewA4 failure with new_document == true does not double-count the 8 already-completed pages.
        panel.state_mut().1.clear();
        panel.state_mut().0.new_document = true;
        panel.state_mut().0.continue_book = false;
        panel.state_mut().0.pages = 20;
        panel.state_mut().0.settings.budget.max_requests = 8;
        panel.state_mut().0.snapshot.batch.as_mut().unwrap().cumulative_usage.requests = 8;
        panel.run_steps(3);
        panel.get_by_label("Set request allowance for remaining book").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.max_requests, 20, "8 consumed + 12 remaining = 20 without double-counting pages 1..8");

        // 4. Once retryable becomes false (fresh task with new ledger), targets revert to 2 / 12 and fresh token estimates.
        panel.state_mut().1.clear();
        panel.state_mut().0.new_document = false;
        panel.state_mut().0.continue_book = true;
        panel.state_mut().0.snapshot.batch.as_mut().unwrap().retryable = false;
        panel.state_mut().0.settings.budget.max_requests = 1;
        panel.state_mut().0.settings.budget.token_limit = 1_000;
        panel.run_steps(3);
        assert!(panel.query_by_label("Configured request limit 1 is below the conservative planning allowance for this review: 2.").is_some());
        panel.get_by_label("Set request allowance for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.max_requests, 2);
        panel.get_by_label("Set request allowance for remaining book").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.max_requests, 12);

        panel.get_by_label("Set token limit for this review").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.token_limit, fresh_tokens.review_tokens);
        panel.get_by_label("Set token limit for remaining book").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.token_limit, fresh_tokens.book_tokens);

        // 5. Edge case A: When budget_targets is None (e.g. CurrentDocument mode) and consumed_tokens >= token_limit * 2,
        //    "Raise Token Limit" grants 256,000 fresh tokens of headroom above consumed_tokens.
        panel.state_mut().1.clear();
        panel.state_mut().0.new_document = false;
        panel.state_mut().0.continue_book = false;
        panel.state_mut().0.settings.budget.token_limit = 140_000;
        {
            let batch = panel.state_mut().0.snapshot.batch.as_mut().unwrap();
            batch.retryable = true;
            batch.cumulative_usage = UsageSummary { requests: 8, input_tokens: 250_000, output_tokens: 50_000, ..Default::default() };
        }
        panel.state_mut().0.snapshot.stage = "failed".into();
        panel.state_mut().0.snapshot.message = "Token budget cannot admit another request".into();
        panel.run_steps(3);
        assert!(budget_targets(&panel.state().0, &panel.state().3, panel.state().2).is_none());
        panel.get_by_label("Raise Token Limit").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.token_limit, 556_000, "300_000 consumed + 256_000 fallback headroom = 556_000");
        assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.token_limit == 556_000));

        // 6. Edge case B: Unpublished NewA4 retryable batch (total_pages = 20, completed_pages = 0, document_uid = None)
        //    while new_document == false on a smaller open document (remaining_pages = 4) still uses batch.total_pages (20), not 4.
        panel.state_mut().1.clear();
        {
            let state = &mut panel.state_mut().0;
            state.new_document = false;
            state.continue_book = true;
            state.settings.budget.max_requests = 4;
            let draft = state.snapshot.production_draft.as_mut().unwrap();
            draft.request.target = TaskTarget::NewA4 { pages: 20 };
            draft.document_uid = None;
            draft.document_path = None;
            let batch = state.snapshot.batch.as_mut().unwrap();
            batch.completed_pages = 0;
            batch.total_pages = 20;
            batch.pages_per_batch = 2;
            batch.document_uid = None;
            batch.retryable = true;
            batch.cumulative_usage.requests = 4;
        }
        panel.state_mut().3[0].pages = 6;
        panel.state_mut().3[0].remaining_pages = 4;
        panel.run_steps(3);
        panel.get_by_label("Set request allowance for remaining book").click();
        panel.run_steps(3);
        assert_eq!(panel.state().0.settings.budget.max_requests, 24, "4 consumed + 20 unpublished NewA4 batch pages = 24 (not 4 + 4 = 8)");
        assert!(matches!(panel.state().1.last(), Some(AiRequest::UpdateSettings(s)) if s.budget.max_requests == 24));
    }

    #[test]
    fn ai28_ui_refresh_edited_proofs_retired_gemini_and_bound_draft_pages() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;

        // 1. Expanded retired Gemini detection (gemini-3-pro-preview, imagen-3.0, gemini-2.5-flash).
        assert!(is_retired_gemini_text_model(ProviderKind::Gemini, "gemini-3-pro-preview"));
        assert!(is_retired_gemini_text_model(ProviderKind::Gemini, "imagen-3.0-generate-002"));
        assert!(is_retired_gemini_text_model(ProviderKind::Gemini, "models/gemini-3.1-flash-lite-preview"));
        assert!(!is_retired_gemini_text_model(ProviderKind::Gemini, "gemini-3.1-pro"));
        assert!(!is_retired_gemini_text_model(ProviderKind::Gemini, "gemini-4-argon"));

        // 2. 404 "model or endpoint not found" maps to InvalidModel.
        assert_eq!(
            classify_diagnostic_alert("Provider HTTP 404: model or endpoint not found (`gemini-2.5-flash`).").kind,
            DiagnosticAlertKind::InvalidModel
        );

        // 3. Bound ContinueBook draft restores saved target_total_pages on accept().
        let mut init_state = AiUiState::default();
        init_state.accept(AiSnapshot {
            available: true,
            production_draft: Some(designcraft_ai_types::protocol::ProductionDraft {
                id: "bound-32".into(),
                request: TaskRequest {
                    prompt: "Continue 32-page book".into(),
                    settings: AiSettings::default(),
                    skill_ids: vec![],
                    auto_select_skills: false,
                    target: TaskTarget::ContinueBook,
                    export_directory: None,
                },
                document_uid: Some(77),
                document_path: None,
                document_title: "Book 32".into(),
                progress: designcraft_ai_types::protocol::SavedProductionProgress { completed_pages: 4, target_total_pages: Some(32) },
            }),
            ..Default::default()
        });
        assert!(init_state.continue_book);
        assert!(!init_state.new_document);
        assert_eq!(init_state.pages, 32);

        // 4. Refresh proofs button appears when the active document has the same UID but a newer edited revision.
        let mut fixture = ProofFixture::new();
        let document = fixture.app.ui.ai.proof_key.as_ref().unwrap().document;
        fixture.app.ui.ai.sync_proof_document(Some((document.0, document.1 + 1)));
        assert!(!fixture.app.ui.ai.proof_document_matches);

        let mut harness = Harness::new_ui_state(
            |ui, (s, reqs): &mut (AiUiState, Vec<AiRequest>)| {
                batch_review(ui, s, false, reqs);
            },
            (fixture.app.ui.ai, Vec::<AiRequest>::new()),
        );
        harness.run_steps(2);
        harness.get_by_label("Refresh proofs for edited document").click();
        harness.run_steps(2);
        assert!(matches!(
            harness.state().1.last(),
            Some(AiRequest::RefreshBatchProofs { job_id }) if job_id == "two-page-proof"
        ));
    }

    #[test]
    fn ai28_round1_review_fixes_16m_tokens_spread_aligned_estimates_and_http_alerts() {
        use egui_kittest::Harness;
        use egui_kittest::kittest::Queryable;

        // 1. HTTP 504 / 429 / 400 / 422 diagnostic alert classifications.
        assert_eq!(
            classify_diagnostic_alert("AI provider HTTP 504: provider temporarily unavailable").kind,
            DiagnosticAlertKind::ConnectionOrRateLimit
        );
        assert_eq!(classify_diagnostic_alert("AI provider HTTP 429: rate or quota limit reached").kind, DiagnosticAlertKind::ConnectionOrRateLimit);
        assert_eq!(
            classify_diagnostic_alert("AI provider HTTP 400: invalid request or unsupported model parameter").kind,
            DiagnosticAlertKind::General
        );
        assert_eq!(classify_diagnostic_alert("AI provider HTTP 422: request schema or parameters rejected").kind, DiagnosticAlertKind::General);

        // 2. 32-page facing_pages=true, pages_per_batch=2: cover Batch 1 is 1 page out of 17 batches.
        let mut s = AiUiState {
            new_document: true,
            continue_book: false,
            pages: 32,
            prompt: "Design a 32-page illustrated science book.".into(),
            ..Default::default()
        };
        s.settings.facing_pages = true;
        s.settings.pages_per_batch = 2;
        s.settings.output_mode = OutputMode::NativeOnly;
        let est = s.request_estimate(&[], None).expect("request estimate");
        assert_eq!(est.review_pages, 1);
        assert_eq!(est.remaining_groups, 17);

        s.snapshot.stage = "planning".into();
        s.snapshot.message = "Preparing native production context".into();
        s.snapshot.batch = Some(BatchSnapshot {
            job_id: "spread-32".into(),
            status: "running".into(),
            total_pages: 32,
            completed_pages: 0,
            pages_per_batch: 2,
            spread_aligned: true,
            current_batch: 1,
            total_batches: 17,
            ..Default::default()
        });
        let en_line = s.live_status_line(false);
        let ar_line = s.live_status_line(true);
        assert!(en_line.contains("Batch 1 / 17 · Page 1 / 32"), "en_line={en_line}");
        assert!(ar_line.contains("الدفعة 1 / 17 · الصفحة 1 / 32"), "ar_line={ar_line}");

        // Failed retryable Batch 1 on spread_aligned book uses 1 page (not 2) in budget_targets_for_estimate.
        s.snapshot.stage = "failed".into();
        s.snapshot.message = "AI request limit reached".into();
        s.snapshot.production_draft = Some(ProductionDraft {
            id: "spread-32".into(),
            request: TaskRequest {
                prompt: s.prompt.clone(),
                settings: s.settings.clone(),
                skill_ids: vec![],
                auto_select_skills: false,
                target: TaskTarget::NewA4 { pages: 32 },
                export_directory: None,
            },
            document_uid: None,
            document_path: None,
            document_title: "Book 32".into(),
            progress: Default::default(),
        });
        if let Some(batch) = s.snapshot.batch.as_mut() {
            batch.status = "failed".into();
            batch.retryable = true;
            batch.cumulative_usage.requests = 2;
        }
        let targets = budget_targets(&s, &[], None).expect("budget targets");
        assert_eq!(targets.review_requests, 3, "2 consumed + 1 spread-aligned cover page = 3");

        // 3. image_call_tokens matches ledger.rs (3_072 without workflow/verify) & 16M token ceiling scales above 4M.
        s.settings.verify_generated_images = false;
        let native_only_tokens = s.token_estimate(&[], None).unwrap().per_page_tokens;
        s.settings.output_mode = OutputMode::NativeWithImages;
        let with_images_tokens = s.token_estimate(&[], None).unwrap().per_page_tokens;
        assert_eq!(with_images_tokens - native_only_tokens, 3_072);

        s.pages = 120;
        s.settings.budget.max_output_tokens = 32_768;
        s.settings.budget.token_limit = 3_000_000;
        let large_tokens = s.token_estimate(&[], None).unwrap();
        assert!(large_tokens.book_tokens > 4_000_000 && large_tokens.book_tokens <= MAX_TOKEN_LIMIT, "book_tokens={}", large_tokens.book_tokens);
        let large_targets = budget_targets(&s, &[], None).unwrap();
        assert!(large_targets.book_tokens.unwrap() > 4_000_000);

        s.snapshot.stage = "failed".into();
        s.snapshot.message = "Token budget cannot admit another request".into();
        let mut harness = Harness::builder().with_size(egui::vec2(1200.0, 900.0)).build_ui_state(
            |ui, (st, reqs): &mut (AiUiState, Vec<AiRequest>)| {
                render_diagnostic_alert_banner(ui, st, false, &[], None, reqs);
            },
            (s, Vec::<AiRequest>::new()),
        );
        harness.run_steps(2);
        harness.get_by_label("Raise Token Limit").click();
        harness.run_steps(2);
        assert!(
            harness.state().0.settings.budget.token_limit >= 6_000_000 && harness.state().0.settings.budget.token_limit <= MAX_TOKEN_LIMIT,
            "raised={}",
            harness.state().0.settings.budget.token_limit
        );
    }

    #[test]
    fn ai28_package5_settings_sync_output_mode_language_toggle_and_bidi_layouter() {
        use egui_kittest::kittest::Queryable;

        // 1. Inactive draft preserves saved user settings (models, budget, image_rates) while keeping draft batch/page settings.
        let mut saved_settings = AiSettings::default();
        saved_settings.text_model.model = "gemini-3.8-flash".into();
        saved_settings.budget.max_requests = 48;
        saved_settings.budget.rates = Some(TokenRates {
            input_per_million: 1.25,
            cached_input_per_million: 0.3,
            output_per_million: 5.0,
            cache_write_per_million: None,
            image_per_request: None,
            image_reserve_usd: None,
        });
        saved_settings.image_rates = Some(TokenRates {
            input_per_million: 0.0,
            cached_input_per_million: 0.0,
            output_per_million: 0.0,
            cache_write_per_million: None,
            image_per_request: Some(0.04),
            image_reserve_usd: None,
        });

        let mut draft_req = continuation_request(TaskTarget::NewA4 { pages: 12 });
        draft_req.settings.output_mode = OutputMode::NativeWithImages;
        draft_req.settings.text_model.model = "gemini-2.5-flash".into();
        draft_req.settings.budget.max_requests = 4;

        let mut state = AiUiState::default();
        state.accept(AiSnapshot {
            available: true,
            settings: saved_settings,
            production_draft: Some(ProductionDraft {
                id: "old-draft".into(),
                request: draft_req,
                document_uid: None,
                document_path: None,
                document_title: "Draft".into(),
                progress: Default::default(),
            }),
            ..Default::default()
        });
        assert_eq!(state.settings.text_model.model, "gemini-3.8-flash");
        assert_eq!(state.settings.budget.max_requests, 48);
        assert_eq!(state.settings.pages_per_batch, 3);
        assert!(state.rates_confirmed, "image_per_request + text rates satisfy NativeWithImages rate confirmation");

        // 2. OutputMode toggle in main Tab 0 + 1-click language switch button in AI window top bar.
        let mut app = DesignApp::new(
            Session::new(),
            Services { ai: Some(Box::new(|_, _| AiSnapshot { available: true, ..Default::default() })), ..Default::default() },
        );
        app.ui.ai.open = true;
        app.ui.ai.snapshot.available = true;
        app.ui.ai.initialized = true;
        app.ui.ai.prompt = "صمّم كراسة علوم تفاعلية للأطفال من 8 صفحات".into();

        let mut win = crate::test_window::open(app, egui::vec2(1280.0, 960.0));
        win.run_steps(3);
        assert_eq!(win.state().app.ui.ai.settings.output_mode, OutputMode::NativeOnly);
        win.get_by_label("Editable elements and images").click();
        win.run_steps(3);
        assert_eq!(win.state().app.ui.ai.settings.output_mode, OutputMode::NativeWithImages);

        // Single-page mode also exposes Book binding (RTL/LTR/Automatic).
        assert!(!win.state().app.ui.ai.settings.facing_pages);
        assert_eq!(win.state().app.ui.ai.settings.right_to_left_binding, None);
        win.get_by_label("Right to left (RTL)").click();
        win.run_steps(3);
        assert_eq!(win.state().app.ui.ai.settings.right_to_left_binding, Some(true));

        // Both the top app bar and the AI window expose the 1-click language toggle.
        let toggles: Vec<_> = win.query_all_by_label("العربية").collect();
        assert_eq!(toggles.len(), 2);
        toggles.into_iter().last().unwrap().click();
        win.run_steps(3);
        assert_eq!(win.state().app.ui.language, "ar");
        win.query_all_by_label("EN").next().unwrap().click();
        win.run_steps(3);
        assert_eq!(win.state().app.ui.language, "");

        // 3. Single-page batch review exposes the 100% image preview toggle even when facing_pages == false.
        let mut fixture = ProofFixture::new();
        fixture.app.ui.ai.settings.facing_pages = false;
        let mut single_review = fixture.panel();
        single_review.run_steps(3);
        assert!(single_review.query_by_label("Show facing pair").is_none(), "single-page review does not offer facing pairs");
        assert!(!single_review.state().ui.ai.proof_full_resolution);
        single_review.get_by_label("100% image preview").click();
        single_review.run_steps(3);
        assert!(single_review.state().ui.ai.proof_full_resolution, "single-page review must allow 100% image inspection");
    }

    #[test]
    fn control_channel_ai_request_cannot_escalate_spending_budget_above_user_ceiling() {
        let persisted_rates = TokenRates {
            input_per_million: 2.50,
            cached_input_per_million: 0.25,
            output_per_million: 10.0,
            cache_write_per_million: None,
            image_per_request: Some(0.08),
            image_reserve_usd: None,
        };
        let mut persisted_ceiling = AiSettings::default();
        persisted_ceiling.budget.max_requests = 6;
        persisted_ceiling.budget.token_limit = 120_000;
        persisted_ceiling.budget.max_cost_usd = Some(1.50);
        persisted_ceiling.budget.rates = Some(persisted_rates.clone());
        persisted_ceiling.allow_paid_repairs = false;
        persisted_ceiling.output_mode = OutputMode::NativeOnly;
        persisted_ceiling.verify_generated_images = false;

        let host_settings = std::rc::Rc::new(std::cell::RefCell::new(persisted_ceiling));
        let host_settings_ref = host_settings.clone();
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen_host = seen.clone();
        let mut app = DesignApp::new(
            Session::new(),
            Services {
                ai: Some(Box::new(move |_, req| {
                    if let AiRequest::UpdateSettings(s) = &req {
                        seen_host.borrow_mut().push(s.clone());
                        *host_settings_ref.borrow_mut() = s.clone();
                    }
                    AiSnapshot { available: true, settings: host_settings_ref.borrow().clone(), ..Default::default() }
                })),
                ..Default::default()
            },
        );
        // Verify uninitialized `app.ui.ai` hydrates persisted runtime ceiling BEFORE clamping!
        assert!(!app.ui.ai.initialized);

        let mut escalated = AiSettings::default();
        escalated.budget.max_requests = 9_999;
        escalated.budget.token_limit = 15_000_000;
        escalated.budget.max_cost_usd = None;
        escalated.budget.rates = Some(TokenRates {
            input_per_million: 1e-9,
            cached_input_per_million: 1e-9,
            output_per_million: 1e-9,
            cache_write_per_million: None,
            image_per_request: Some(1e-9),
            image_reserve_usd: None,
        });
        escalated.allow_paid_repairs = true;
        escalated.output_mode = OutputMode::NativeWithImages;
        escalated.verify_generated_images = true;

        app.run("ai.request", json!({"method": "update_settings", "settings": escalated})).unwrap();
        assert!(app.ui.ai.initialized, "request_command must hydrate uninitialized settings first");
        let applied = seen.borrow().last().cloned().expect("settings forwarded");
        assert_eq!(applied.budget.max_requests, 6);
        assert_eq!(applied.budget.token_limit, 120_000);
        assert_eq!(applied.budget.max_cost_usd, Some(1.50));
        assert!(!applied.allow_paid_repairs);
        assert_eq!(applied.output_mode, OutputMode::NativeOnly);
        assert!(!applied.verify_generated_images);
        assert_eq!(applied.budget.rates.as_ref().map(|r| r.input_per_million), Some(2.50));

        // Tightening below the ceiling remains allowed while preserving user rate tables.
        let mut tighter = app.ui.ai.settings.clone();
        tighter.budget.max_requests = 2;
        tighter.budget.token_limit = 50_000;
        tighter.budget.max_cost_usd = Some(0.50);
        app.run("ai.request", json!({"method": "update_settings", "settings": tighter})).unwrap();
        let applied = seen.borrow().last().cloned().expect("tighter settings forwarded");
        assert_eq!(applied.budget.max_requests, 2);
        assert_eq!(applied.budget.token_limit, 50_000);
        assert_eq!(applied.budget.max_cost_usd, Some(0.50));
        assert_eq!(applied.budget.rates.as_ref().map(|r| r.output_per_million), Some(10.0));
    }
    #[test]
    fn ai31_arabic_polish_bar_executes_whole_book_polish_in_one_undo_step() {
        let mut app = DesignApp::new(designcraft_engine::Session::new(), Default::default());
        app.run("file.new", json!({"preset": "A4", "pages": 2, "facingPages": true, "binding": "rightToLeft"})).unwrap();
        app.run("frame.create", json!({"spread": 0, "rect": [80.0, 80.0, 400.0, 160.0], "content": "text", "text": "السَّلَامُ عَلَيْكُمْ ﷺ"})).unwrap();
        app.ui.ai.settings.arabic_tashkeel = ArabicTashkeelMode::None;
        let mut requests = Vec::new();
        let res = run_polish_current_arabic_book(&mut app, true, &mut requests).unwrap();
        assert!(res["diacriticsRemoved"].as_u64().unwrap() > 0);
        assert!(app.ui.ai.polish_book_status.contains("تم تحسين الكتاب"));
        let doc = &app.session.active().unwrap().doc;
        let text = &doc.stories.values().next().unwrap().text;
        assert!(text.contains("السلام عليكم"));
        assert!(text.contains('ﷺ'));
        app.run("edit.undo", json!({})).unwrap();
        let undone_text = &app.session.active().unwrap().doc.stories.values().next().unwrap().text;
        assert!(undone_text.contains('\u{064E}'));
    }
}
