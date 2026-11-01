use super::{
    AiSettings, Arc, AtomicBool, BTreeMap, BTreeSet, BatchScope, DocState, Engine, EngineAction, GeometryCheck, MAX_ACTIONS, MAX_GROUP_PLAN_BYTES,
    MAX_PROOF_BYTES, MAX_PROOFS_BYTES, MAX_REVIEW_PAGES, ModelSelection, OutputMode, Prefs, ProductionPlan, ProofArtifact, ProviderKind, Session,
    SkillSnapshot, TaskRequest, Value, batch_command, bounded_json, check_cancel, json, reference_source,
};

pub(super) fn model_needs_thinking_margin(selection: &ModelSelection) -> bool {
    let normalized = selection.model.trim().trim_start_matches("models/").to_ascii_lowercase();
    let leaf = normalized.rsplit('/').next().unwrap_or(normalized.as_str());
    match selection.provider {
        ProviderKind::Gemini => true,
        ProviderKind::OpenAi => leaf.starts_with("o1") || leaf.starts_with("o3") || leaf.starts_with("o4") || leaf.starts_with("gpt-5"),
        ProviderKind::Ollama | ProviderKind::LocalOpenAi => {
            leaf.starts_with("deepseek-r1")
                || leaf.starts_with("qwq")
                || leaf.starts_with("qwen3")
                || leaf.starts_with("gpt-oss")
                || leaf.contains("reasoning")
                || leaf.contains("think")
        }
        _ => false,
    }
}

pub(crate) fn is_token_truncation_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("max_tokens")
        || lower.contains("finish_reason: length")
        || lower.contains("finishreason: max_tokens")
        || lower.contains("truncated")
        || lower.contains("eof while parsing")
        || lower.contains("unterminated string")
}

pub(super) fn arabic_vowel_mark(c: char) -> bool {
    matches!(c, '\u{064b}'..='\u{0652}' | '\u{0670}')
}

pub(super) fn review_end(scope: BatchScope) -> Result<u32, String> {
    scope
        .start
        .checked_add(scope.count)
        .filter(|end| scope.count > 0 && scope.count <= MAX_REVIEW_PAGES && *end <= MAX_REVIEW_PAGES)
        .ok_or_else(|| "Review must contain 1 to 1000 native pages within the document".into())
}

/// Existing-book trim and margins outrank fixed template coordinates. Matching
/// books retain the local compiler; different geometry uses native adaptive
/// commands with the same immutable visual guidance.
pub(super) fn continuation_style_geometry_differs(request: &TaskRequest, base: &DocState, skills: &[SkillSnapshot]) -> bool {
    matches!(request.target, designcraft_ai_types::protocol::TaskTarget::ContinueBook)
        && skills.iter().filter_map(|skill| skill.design_profile.as_ref()).any(|profile| {
            let factor = 72.0 / 25.4;
            base.doc.spreads.iter().flat_map(|spread| &spread.pages).any(|page| {
                (page.width - profile.page_mm[0] * factor).abs() > 0.1
                    || (page.height - profile.page_mm[1] * factor).abs() > 0.1
                    || [page.margins.top, page.margins.bottom, page.margins.inside, page.margins.outside]
                        .iter()
                        .any(|margin| *margin > profile.margin_mm * factor + 0.1)
            })
        })
}

pub(super) fn unbound_math_request(brief: &str, skills: &[SkillSnapshot]) -> bool {
    crate::template_plan::requires_adaptive_mastermind(brief, skills)
}
pub(super) fn generic_page_capacity(mode: OutputMode) -> u32 {
    match mode {
        OutputMode::NativeOnly => 32,
        OutputMode::NativeWithImages => 4,
    }
}

pub(super) fn opened_document_geometry(context: &mut Value, state: &DocState) -> Result<(), String> {
    let object = context.as_object_mut().ok_or("Invalid current-document context")?;
    object.insert("pageWidth".into(), json!(state.doc.settings.page_width));
    object.insert("pageHeight".into(), json!(state.doc.settings.page_height));
    object.insert("pageCount".into(), json!(state.doc.page_count()));
    object.insert("facingPages".into(), json!(state.doc.settings.facing_pages));
    object.insert("rightToLeftBinding".into(), json!(state.doc.settings.right_to_left_binding));
    let pages = state
        .doc
        .spreads
        .iter()
        .enumerate()
        .flat_map(|(spread, entry)| entry.pages.iter().map(move |page| (spread, page)))
        .take(MAX_REVIEW_PAGES as usize)
        .enumerate()
        .map(|(absolute, (spread, page))| {
            let safe = page.margin_rect();
            json!({"page":absolute,"spread":spread,"side":page.side,"x":page.x,"width":page.width,"height":page.height,
                "margins":page.margins,"safeRect":[safe.x0,safe.y0,safe.x1,safe.y1]})
        })
        .collect::<Vec<_>>();
    object.insert("pageGeometryTruncated".into(), json!(state.doc.page_count() > pages.len()));
    object.insert("pageGeometry".into(), json!(pages));
    Ok(())
}

pub(crate) fn prompt_requests_document_geometry_edit(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    const PATTERNS: &[&str] = &[
        "add page",
        "add a page",
        "add pages",
        "insert page",
        "insert pages",
        "new page",
        "extra page",
        "more pages",
        "margin",
        "margins",
        "page size",
        "page setup",
        "document setup",
        "bleed",
        "slug",
        "facing pages",
        "landscape",
        "portrait",
        "column",
        "columns",
        "أضف صفحة",
        "اضف صفحة",
        "إضافة صفحة",
        "اضافة صفحة",
        "إضافة صفحات",
        "اضافة صفحات",
        "صفحة جديدة",
        "صفحات إضافية",
        "صفحات اضافية",
        "هوامش",
        "الهامش",
        "هامش",
        "حجم الصفحة",
        "مقاس الصفحة",
        "إعداد المستند",
        "اعداد المستند",
        "أعمدة",
        "اعمدة",
    ];
    PATTERNS.iter().any(|pattern| lower.contains(pattern))
}

pub(super) fn check_opened_document_geometry_with_policy(before: &DocState, after: &DocState, allow_geometry_edits: bool) -> Result<(), String> {
    if allow_geometry_edits {
        if after.doc.page_count() < before.doc.page_count() || after.doc.spreads.len() < before.doc.spreads.len() {
            return Err("Opened document geometry edits cannot delete existing pages or spreads".into());
        }
        for (old, new) in before.doc.spreads.iter().zip(&after.doc.spreads) {
            if new.pages.len() < old.pages.len() {
                return Err("Opened document geometry edits cannot delete pages from existing spreads".into());
            }
        }
        return Ok(());
    }
    check_opened_document_geometry(before, after)
}

pub(super) fn check_opened_document_geometry(before: &DocState, after: &DocState) -> Result<(), String> {
    let old = &before.doc.settings;
    let new = &after.doc.settings;
    if old.page_width != new.page_width
        || old.page_height != new.page_height
        || old.facing_pages != new.facing_pages
        || old.right_to_left_binding != new.right_to_left_binding
        || old.bleed != new.bleed
        || old.slug != new.slug
    {
        return Err("Opened document geometry must preserve native size, facing pages, binding, bleed and slug".into());
    }
    if before.doc.spreads.len() != after.doc.spreads.len() || before.doc.page_count() != after.doc.page_count() {
        return Err("Opened document geometry must preserve existing pages and spread grouping".into());
    }
    for (old, new) in before.doc.spreads.iter().zip(&after.doc.spreads) {
        if old.pages.len() != new.pages.len() {
            return Err("Opened document geometry must preserve existing spread grouping".into());
        }
        // Document Setup may rebuild SpreadIds during a harmless repagination.
        // Existing PageIds, their grouping and their geometry are authoritative.
        for (old, new) in old.pages.iter().zip(&new.pages) {
            if old.id != new.id || old.width != new.width || old.height != new.height || old.x != new.x || old.margins != new.margins {
                return Err("Opened document geometry must preserve page identity, order, size, position and margins".into());
            }
        }
    }
    Ok(())
}

pub(super) fn internal_template_context(context: &Value, state: &DocState, review: BatchScope, scope: BatchScope) -> Result<Value, String> {
    let mut value = context.clone();
    let object = value.as_object_mut().ok_or("Invalid internal review context")?;
    object.remove("batch_scope");
    object.insert("review_scope".into(), json!(review));
    object.insert("internal_scope".into(), json!(scope));
    object.insert("internalProducedPageCount".into(), json!(scope.start.checked_sub(review.start).ok_or("Invalid internal review scope")?));
    object.insert("documentUid".into(), json!(state.uid));
    object.insert("revision".into(), json!(state.revision));
    object.insert("pageCount".into(), json!(state.doc.page_count()));
    object.insert("pageWidth".into(), json!(state.doc.settings.page_width));
    object.insert("pageHeight".into(), json!(state.doc.settings.page_height));
    let mut objects = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;
    let recent_start = review.start.max(scope.start.saturating_sub(32));
    'pages: for page in (recent_start..scope.start).rev() {
        let spread =
            state.doc.spreads.get(usize::try_from(page).map_err(|_| "Invalid internal context page")?).ok_or("Internal review page disappeared")?;
        for item in &spread.items {
            let bounds = item.bounds();
            let text =
                item.text_frame().and_then(|frame| state.doc.stories.get(&frame.story)).map(|story| story.text.chars().take(160).collect::<String>());
            let entry = json!({"spread":page,"label":item.label.chars().take(48).collect::<String>(),"bounds":[bounds.x0,bounds.y0,bounds.x1,bounds.y1],"text":text});
            let length = serde_json::to_vec(&entry).map_err(|e| e.to_string())?.len();
            if objects.len() >= 64 || bytes.saturating_add(length) > 10_000 {
                truncated = true;
                break 'pages;
            }
            bytes = bytes.saturating_add(length);
            objects.push(entry);
        }
    }
    object.insert("objects".into(), json!(objects));
    object.insert("objectsTruncated".into(), json!(truncated));
    object.insert("internalContentInstruction".into(), json!("Prior internal pages are actual private native draft content, awaiting the same human review. Continue the full requested review scope, create only the supplied internal scope, and preserve earlier internal pages. Internal parts are never human approvals."));
    Ok(value)
}

pub(super) fn identical_layout_requested(lower: &str) -> bool {
    ["نفس التخطيط", "نفس التوزيع", "تخطيط متطابق", "قالب متكرر", "same layout", "identical layouts", "repeat layout", "identical worksheets"]
        .iter()
        .any(|phrase| lower.contains(phrase))
}

/// Infer only a unique declared family whose static labels survive in native content.
/// Missing or ambiguous labels are unknown; never manufacture approved history.
pub(super) fn template_family_for_labels(style: &SkillSnapshot, labels: &BTreeSet<&str>, allowed: &BTreeSet<String>) -> Option<String> {
    let mut matched = None;
    for family in &style.design_profile.as_ref()?.page_families {
        if !allowed.contains(family.id.as_str()) {
            continue;
        }
        let template = style.templates.get(&family.template)?;
        let expected: BTreeSet<_> = template
            .iter()
            .filter(|action| action.command == "object.label")
            .filter_map(|action| action.params.get("label").and_then(Value::as_str))
            .filter(|label| !label.is_empty())
            .collect();
        if !expected.is_empty() && expected.is_subset(labels) {
            if matched.is_some() {
                return None;
            }
            matched = Some(family.id.clone());
        }
    }
    matched
}

pub(super) fn previous_template_family(
    style: &SkillSnapshot,
    base: &DocState,
    scope: BatchScope,
    allowed: &BTreeSet<String>,
) -> Result<Option<String>, String> {
    let Some(previous) = scope.start.checked_sub(1).filter(|_| allowed.len() > 1) else { return Ok(None) };
    let spread =
        base.doc.spreads.get(usize::try_from(previous).map_err(|_| "Invalid previous template page")?).ok_or("Previous template page is missing")?;
    let mut pending = Vec::new();
    for item in &spread.items {
        if pending.len() >= 10_000 {
            return Err("Template family traversal exceeds its limit".into());
        }
        pending.push((item.as_ref(), 0usize));
    }
    let mut labels = BTreeSet::new();
    let mut visited = 0usize;
    while let Some((item, depth)) = pending.pop() {
        visited = visited.saturating_add(1);
        if visited > 10_000 || depth > 64 {
            return Err("Template family traversal exceeds its limit".into());
        }
        if !item.label.is_empty() {
            labels.insert(item.label.as_str());
        }
        for child in item.children() {
            if pending.len() >= 10_000 {
                return Err("Template family traversal exceeds its limit".into());
            }
            pending.push((child.as_ref(), depth.saturating_add(1)));
        }
    }
    Ok(template_family_for_labels(style, &labels, allowed))
}

pub(super) fn validate_template_family_sequence(
    plan: &ProductionPlan,
    style: &SkillSnapshot,
    mut previous: Option<String>,
    scope: BatchScope,
    allowed: &BTreeSet<String>,
    brief: &str,
) -> Result<(), String> {
    if allowed.len() < 2 || identical_layout_requested(&brief.to_lowercase()) {
        return Ok(());
    }
    if plan.commands.len() > MAX_ACTIONS {
        return Err("Template family commands exceed their limit".into());
    }
    let mut results: Vec<BTreeSet<u64>> = Vec::with_capacity(plan.commands.len());
    let mut labels: BTreeMap<u64, BTreeSet<&str>> = BTreeMap::new();
    for (index, action) in plan.commands.iter().enumerate() {
        let mut pages = BTreeSet::new();
        if matches!(action.command.as_str(), "frame.create" | "line.create" | "path.create" | "file.place") {
            pages.insert(action.params.get("spread").and_then(Value::as_u64).unwrap_or(0));
        } else if let Some(ids) = action.params.get("ids").and_then(Value::as_array) {
            for reference in ids.iter().filter_map(Value::as_str) {
                if let Some(source) = reference_source(reference, index).and_then(|source| results.get(source)) {
                    pages.extend(source);
                }
            }
        }
        if action.command == "object.label"
            && let Some(label) = action.params.get("label").and_then(Value::as_str).filter(|label| !label.is_empty())
        {
            for page in &pages {
                labels.entry(*page).or_default().insert(label);
            }
        }
        results.push(pages);
    }
    for page in scope.start..review_end(scope)? {
        let family = labels.get(&u64::from(page)).and_then(|labels| template_family_for_labels(style, labels, allowed));
        if let Some(family) = &family
            && previous.as_ref() == Some(family)
        {
            return Err(format!(
                "Repeated template family '{family}' on page {}; choose another available family or explicitly request identical layouts. No images were requested.",
                page.saturating_add(1)
            ));
        }
        previous = family;
    }
    Ok(())
}

/// Local editorial checks run before image reservations on either production path.
pub(super) fn validate_previous_body_layout(
    plan: &ProductionPlan,
    base: &DocState,
    prefs: &Prefs,
    scope: Option<BatchScope>,
    prompt: &str,
) -> Result<(), String> {
    let Some(scope) = scope.filter(|scope| scope.start > 0 || scope.count > 1) else { return Ok(()) };
    let first = scope.start.saturating_sub(1);
    let end = review_end(scope)?;
    let mut scratch = Session::new();
    scratch.prefs = prefs.clone();
    scratch.add_document(base.clone());
    scratch
        .execute("production.apply", &json!({"commands":plan.commands,"outputMode":"native_only","requirePreflight":false}))
        .map_err(|error| error.to_string())?;
    let state = scratch.doc().map_err(|error| error.to_string())?;
    let mut combined = ProductionPlan { title: String::new(), commands: Vec::new(), images: Vec::new() };
    let mut visited = 0usize;
    for page in first..end {
        let spread = state.doc.spreads.get(usize::try_from(page).map_err(|_| "Invalid native page index")?).ok_or("Native layout page is missing")?;
        let mut pending = Vec::new();
        for item in &spread.items {
            if pending.len() >= 10_000 {
                return Err("Native layout traversal exceeds its limit".into());
            }
            pending.push((item.as_ref(), designcraft_geom::Affine::IDENTITY, 0usize));
        }
        while let Some((item, parent, depth)) = pending.pop() {
            visited = visited.saturating_add(1);
            if visited > 10_000 || depth > 64 {
                return Err("Native layout traversal exceeds its limit".into());
            }
            if item.hidden {
                continue;
            }
            if !item.children().is_empty() {
                for child in item.children() {
                    if pending.len() >= 10_000 {
                        return Err("Native layout traversal exceeds its limit".into());
                    }
                    pending.push((child.as_ref(), parent * item.xf, depth.saturating_add(1)));
                }
            } else {
                let bounds = parent.transform_rect_bbox(item.bounds());
                let image_slot = plan.images.iter().any(|image| {
                    u32::try_from(image.page).ok() == Some(page)
                        && image
                            .all_rects()
                            .any(|rect| rect.iter().zip([bounds.x0, bounds.y0, bounds.x1, bounds.y1]).all(|(a, b)| (*a - b).abs() <= 0.01))
                });
                let content = if item.text_frame().is_some() {
                    "text"
                } else if item.graphic().is_some() || image_slot {
                    "graphic"
                } else {
                    "unassigned"
                };
                combined.commands.push(EngineAction {
                    command: "frame.create".into(),
                    params: json!({"spread":page,"content":content,"rect":[bounds.x0,bounds.y0,bounds.x1,bounds.y1]}),
                });
            }
        }
    }
    prepare_editorial_plan(&mut combined, prompt, &json!({}), Some(BatchScope { start: first, count: end.saturating_sub(first) }))
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum StoryKey {
    Step(usize),
    Literal(u64),
}

pub(super) fn resolve_story_key(param: Option<&Value>, current_index: usize) -> Option<StoryKey> {
    if let Some(id) = param.and_then(Value::as_u64) {
        return Some(StoryKey::Literal(id));
    }
    let reference = param.and_then(Value::as_str)?;
    let body = reference.strip_prefix("${").and_then(|s| s.strip_suffix('}')).or_else(|| reference.strip_prefix('$'))?;
    let (_, path) = body.split_once('.')?;
    if path != "story" {
        return None;
    }
    reference_source(reference, current_index).map(StoryKey::Step)
}

pub(super) fn strip_arabic_vowels_with_byte_map(text: &mut String) -> Option<Vec<usize>> {
    if !text.chars().any(arabic_vowel_mark) {
        return None;
    }
    let mut map = vec![0usize; text.len().saturating_add(1)];
    let mut stripped = String::with_capacity(text.len());
    for (byte_idx, ch) in text.char_indices() {
        let new_pos = stripped.len();
        for slot in map.iter_mut().skip(byte_idx).take(ch.len_utf8()) {
            *slot = new_pos;
        }
        if !arabic_vowel_mark(ch) {
            stripped.push(ch);
        }
    }
    if let Some(last) = map.last_mut() {
        *last = stripped.len();
    }
    *text = stripped;
    Some(map)
}

pub(super) fn extract_arabic_fonts_from_prompt(prompt: &str) -> Vec<String> {
    let lower = prompt.to_lowercase();
    let mut found = Vec::new();
    for (needles, canonical) in [
        (&["خط الجزيرة", "الجزيرة", "al-jazeera", "aljazeera"][..], "Al-Jazeera-Arabic"),
        (&["خط سلطان", "سلطاني", "سلطان", "sultan"][..], "Sultan"),
        (&["خط القاهرة", "cairo"][..], "Cairo"),
        (&["خط دبي", "dubai"][..], "Dubai"),
        (&["خط صكال", "صكال مجلة", "sakkal"][..], "Sakkal Majalla"),
        (&["خط أميري", "خط اميري", "amiri"][..], "Amiri"),
        (&["خط تجوال", "tajawal"][..], "Tajawal"),
    ] {
        if needles.iter().any(|kw| lower.contains(kw)) {
            found.push(canonical.to_owned());
        }
    }
    found
}

pub(super) fn resolve_installed_arabic_font_request(base: &DocState, setting_font: Option<&str>, prompt: &str) -> Option<String> {
    let scoped = designcraft_fonts::FontDb::global().scoped(base.doc.font_scope);
    if let Some(explicit) = setting_font.map(str::trim).filter(|s| !s.is_empty())
        && let Some(matched) = scoped.resolve_arabic_font(explicit)
    {
        return Some(matched.regular_family);
    }
    let prompt_candidates = extract_arabic_fonts_from_prompt(prompt);
    for candidate in &prompt_candidates {
        if let Some(matched) = scoped.resolve_arabic_font(candidate) {
            return Some(matched.regular_family);
        }
    }
    let lower = prompt.to_lowercase();
    let asks_arabic_font_upgrade = !prompt_candidates.is_empty()
        || ["تبديل الخط", "تغيير الخط", "الخط العربي", "خط واضح", "مثل خط", "replace arabic font", "change arabic font"]
            .iter()
            .any(|kw| lower.contains(kw));
    if asks_arabic_font_upgrade {
        return scoped.preferred_arabic_font().map(|m| m.regular_family);
    }
    None
}

pub(super) fn explicit_arabic_font_contract(
    base: &DocState,
    setting_font: Option<&str>,
    prompt: &str,
) -> Option<crate::print_contract::PrintContract> {
    let resolved = resolve_installed_arabic_font_request(base, setting_font, prompt)?;
    Some(crate::print_contract::PrintContract { arabic_fonts: vec![resolved], ..Default::default() })
}

pub(super) fn auto_preferred_arabic_font_contract(base: &DocState, plan: &ProductionPlan) -> Option<crate::print_contract::PrintContract> {
    let has_tahoma_arabic = plan.commands.iter().any(|a| {
        a.command == "type.char"
            && a.params.get("attrs").and_then(Value::as_object).is_some_and(|attrs| {
                attrs.get("language").and_then(Value::as_str) == Some("ar")
                    && matches!(attrs.get("fontFamily").and_then(Value::as_str), Some("Tahoma" | "Arial"))
            })
    });
    if !has_tahoma_arabic {
        return None;
    }
    let scoped = designcraft_fonts::FontDb::global().scoped(base.doc.font_scope);
    let preferred = scoped.preferred_arabic_font()?;
    if matches!(preferred.regular_family.as_str(), "Tahoma" | "Arial") {
        return None;
    }
    Some(crate::print_contract::PrintContract { arabic_fonts: vec![preferred.regular_family], ..Default::default() })
}

/// Local editorial checks run before image reservations on either production path.
pub(super) fn prepare_editorial_plan(plan: &mut ProductionPlan, prompt: &str, context: &Value, scope: Option<BatchScope>) -> Result<(), String> {
    prepare_editorial_plan_with_mode(plan, prompt, context, scope, designcraft_ai_types::ArabicTashkeelMode::Auto)
}

pub(super) fn prepare_editorial_plan_with_mode(
    plan: &mut ProductionPlan,
    prompt: &str,
    context: &Value,
    scope: Option<BatchScope>,
    tashkeel_mode: designcraft_ai_types::ArabicTashkeelMode,
) -> Result<(), String> {
    let lower = prompt.to_lowercase();
    if should_strip_arabic_vowels(prompt, &lower, context, tashkeel_mode)? {
        strip_arabic_vowels_from_plan(plan);
    }
    if scope.is_none_or(|scope| scope.count < 2) || identical_layout_requested(&lower) {
        return Ok(());
    }
    verify_non_repeated_body_layouts(plan, prompt)
}

fn should_strip_arabic_vowels(
    prompt: &str,
    lower: &str,
    context: &Value,
    tashkeel_mode: designcraft_ai_types::ArabicTashkeelMode,
) -> Result<bool, String> {
    let has = |words: &[&str]| words.iter().any(|word| lower.contains(word));
    let explicit_removal = has(&[
        "ازل التشكيل",
        "أزل التشكيل",
        "إزالة التشكيل",
        "ازالة التشكيل",
        "حذف التشكيل",
        "الغاء التشكيل",
        "إلغاء التشكيل",
        "remove diacritics",
        "remove vowel marks",
    ]);
    let explicitly_plain = explicit_removal
        || tashkeel_mode == designcraft_ai_types::ArabicTashkeelMode::None
        || has(&[
            "دون تشكيل",
            "بدون تشكيل",
            "بلا تشكيل",
            "غير مشكول",
            "بدون الحركات",
            "دون الحركات",
            "تقليل التشكيل",
            "without diacritics",
            "no diacritics",
            "unvocalized",
        ]);
    let has_vocalization_request = !explicitly_plain
        && (matches!(tashkeel_mode, designcraft_ai_types::ArabicTashkeelMode::Full | designcraft_ai_types::ArabicTashkeelMode::Light)
            || has(&[
                "مشكول",
                "مشكل",
                "مشكّل",
                "التشكيل",
                "تشكيل",
                "مع التشكيل",
                "بالتشكيل",
                "بالحركات",
                "الحركات",
                "حركات الحروف",
                "فتحة",
                "ضمة",
                "كسرة",
                "سكون",
                "شدة",
                "تنوين",
                "تأسيس",
                "نورانية",
                "هجاء",
                "تهجئة",
                "fully vocalized",
                "with diacritics",
                "phonics",
            ]));
    let parsed_script = crate::script_contract::parse(prompt)?;
    let script_requires_verbatim_vocalization = parsed_script
        .as_ref()
        .is_some_and(|s| s.pages.iter().any(|p| p.arabic_print_copy.as_deref().is_some_and(|c| c.chars().any(arabic_vowel_mark))));
    let script_has_vocalized_copy = parsed_script.as_ref().is_some_and(|s| {
        s.pages.iter().any(|p| {
            p.title.chars().any(arabic_vowel_mark)
                || p.arabic_print_copy.as_deref().is_some_and(|c| c.chars().any(arabic_vowel_mark))
                || (p.arabic_print_copy.is_none()
                    && p.full_script
                        .lines()
                        .filter(|line| {
                            let trimmed = line.trim();
                            !trimmed.contains("Illustration Prompt") && Some(trimmed) != p.illustration_prompt.as_deref().map(str::trim)
                        })
                        .any(|line| line.chars().any(arabic_vowel_mark)))
        })
    });
    let plain = if script_requires_verbatim_vocalization {
        false
    } else if explicit_removal || tashkeel_mode == designcraft_ai_types::ArabicTashkeelMode::None {
        true
    } else if script_has_vocalized_copy {
        false
    } else if parsed_script.is_some() {
        explicitly_plain || (!context_has_vocalized_copy(context) && !has_vocalization_request)
    } else {
        explicitly_plain || (!prompt.chars().any(arabic_vowel_mark) && !context_has_vocalized_copy(context) && !has_vocalization_request)
    };
    Ok(plain)
}

fn strip_arabic_vowels_from_plan(plan: &mut ProductionPlan) {
    plan.title.retain(|c| !arabic_vowel_mark(c));
    let mut story_maps: BTreeMap<StoryKey, Vec<usize>> = BTreeMap::new();
    let mut active_text_story: Option<StoryKey> = None;
    for (idx, action) in plan.commands.iter_mut().enumerate() {
        strip_arabic_vowels_in_action(idx, action, &mut story_maps, &mut active_text_story);
    }
}

fn strip_arabic_vowels_in_action(
    idx: usize,
    action: &mut EngineAction,
    story_maps: &mut BTreeMap<StoryKey, Vec<usize>>,
    active_text_story: &mut Option<StoryKey>,
) {
    match action.command.as_str() {
        "frame.create" if action.params.get("content").and_then(Value::as_str) == Some("text") => {
            if let Some(Value::String(text)) = action.params.get_mut("text") {
                if let Some(map) = strip_arabic_vowels_with_byte_map(text) {
                    story_maps.insert(StoryKey::Step(idx), map);
                } else {
                    story_maps.remove(&StoryKey::Step(idx));
                }
            }
            *active_text_story = if action.params.get("caret").and_then(Value::as_bool) == Some(false) { None } else { Some(StoryKey::Step(idx)) };
        }
        "story.setText" => {
            let key = resolve_story_key(action.params.get("story"), idx);
            let maybe_map =
                if let Some(Value::String(text)) = action.params.get_mut("text") { strip_arabic_vowels_with_byte_map(text) } else { None };
            if let Some(key) = key {
                if let Some(map) = maybe_map {
                    story_maps.insert(key, map);
                } else {
                    story_maps.remove(&key);
                }
            }
        }
        "text.thread" => {
            if let Some(from_str) = action.params.get("from").and_then(Value::as_str)
                && let Some(src) = reference_source(from_str, idx)
                && let Some(existing) = story_maps.get(&StoryKey::Step(src)).cloned()
            {
                story_maps.insert(StoryKey::Step(idx), existing);
            }
            *active_text_story = None;
        }
        "story.replaceRange" => {
            remap_story_replace_range_vowels(idx, action, story_maps);
        }
        "text.select" => {
            let key = resolve_story_key(action.params.get("story"), idx);
            *active_text_story = key;
            if let Some(map) = key.and_then(|k| story_maps.get(&k)) {
                for field in ["anchor", "focus"] {
                    if let Some(slot) = action.params.get_mut(field)
                        && let Some(old_offset) = slot.as_u64()
                    {
                        let mapped = usize::try_from(old_offset)
                            .ok()
                            .and_then(|pos| map.get(pos).copied())
                            .unwrap_or_else(|| map.last().copied().unwrap_or(0));
                        *slot = json!(mapped);
                    }
                }
            }
        }
        "text.insert" | "table.insert" => {
            if let Some(Value::String(text)) = action.params.get_mut("text") {
                text.retain(|c| !arabic_vowel_mark(c));
            }
            if let Some(key) = *active_text_story {
                story_maps.remove(&key);
            }
        }
        _ => {
            if let Some(Value::String(text)) = action.params.get_mut("text") {
                text.retain(|c| !arabic_vowel_mark(c));
            }
        }
    }
}

fn remap_story_replace_range_vowels(idx: usize, action: &mut EngineAction, story_maps: &mut BTreeMap<StoryKey, Vec<usize>>) {
    let key = resolve_story_key(action.params.get("story"), idx);
    let old_start = action.params.get("start").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok());
    let old_end = action.params.get("end").and_then(Value::as_u64).and_then(|v| usize::try_from(v).ok()).or(old_start);
    let orig_repl_len = action.params.get("text").and_then(Value::as_str).map(str::len).unwrap_or(0);
    let repl_map = if let Some(Value::String(text)) = action.params.get_mut("text") {
        strip_arabic_vowels_with_byte_map(text).unwrap_or_else(|| (0..=text.len()).collect())
    } else {
        vec![0usize]
    };
    let stripped_repl_len = repl_map.last().copied().unwrap_or(0);
    let Some(key) = key else { return };
    if let Some(map) = story_maps.get(&key).cloned() {
        let map_offset = |pos: usize| map.get(pos).copied().unwrap_or_else(|| map.last().copied().unwrap_or(0));
        if let Some(s) = old_start {
            action.params["start"] = json!(map_offset(s));
        }
        if action.params.get("end").is_some()
            && let Some(e) = old_end
        {
            action.params["end"] = json!(map_offset(e));
        }
        if let (Some(s), Some(e)) = (old_start, old_end)
            && s <= e
            && e < map.len()
        {
            let mapped_start = map_offset(s);
            let mapped_end = map_offset(e);
            let mut next_map = Vec::with_capacity(s.saturating_add(orig_repl_len).saturating_add(map.len().saturating_sub(e)));
            if let Some(prefix) = map.get(..s) {
                next_map.extend_from_slice(prefix);
            }
            for r in &repl_map {
                next_map.push(mapped_start.saturating_add(*r));
            }
            if let Some(suffix) = map.get(e.saturating_add(1)..) {
                for tail in suffix {
                    next_map.push(mapped_start.saturating_add(stripped_repl_len).saturating_add(tail.saturating_sub(mapped_end)));
                }
            }
            story_maps.insert(key, next_map);
        } else {
            story_maps.remove(&key);
        }
    } else if orig_repl_len != stripped_repl_len {
        story_maps.remove(&key);
    }
}

fn verify_non_repeated_body_layouts(plan: &ProductionPlan, prompt: &str) -> Result<(), String> {
    let authored_layouts = crate::script_contract::authored_layout_signatures(prompt)?;
    let mut pages: BTreeMap<u64, BTreeSet<String>> = BTreeMap::new();
    for action in &plan.commands {
        if action.command != "frame.create" {
            continue;
        }
        let (Some(page), Some(rect)) = (action.params.get("spread").and_then(Value::as_u64), action.params.get("rect").and_then(Value::as_array))
        else {
            continue;
        };
        let [Some(left), Some(top), Some(right), Some(bottom)] =
            [rect.first(), rect.get(1), rect.get(2), rect.get(3)].map(|n| n.and_then(Value::as_f64))
        else {
            continue;
        };
        if [left, top, right, bottom].iter().any(|n| !n.is_finite()) || top < 140.0 || bottom > 750.0 || right - left < 100.0 || bottom - top < 36.0 {
            continue;
        }
        let kind = action.params.get("content").and_then(Value::as_str).unwrap_or("shape");
        pages.entry(page).or_default().insert(format!("{kind}:{:.0}:{:.0}:{:.0}:{:.0}", left / 4.0, top / 4.0, right / 4.0, bottom / 4.0));
    }
    let pages = pages.iter().collect::<Vec<_>>();
    for (left, right) in pages.iter().zip(pages.iter().skip(1)) {
        if left.0.saturating_add(1) != *right.0 || left.1.len() < 4 || right.1.len() < 4 {
            continue;
        }
        if authored_layouts.get(left.0).zip(authored_layouts.get(right.0)).is_some_and(|(a, b)| a == b) {
            continue;
        }
        let common = left.1.intersection(right.1).count();
        let union = left.1.union(right.1).count();
        if common.saturating_mul(100) >= union.saturating_mul(85) {
            return Err(format!(
                "Repeated body layout on pages {} and {}; change major body regions or explicitly request identical layouts. No images were requested.",
                left.0.saturating_add(1),
                right.0.saturating_add(1)
            ));
        }
    }
    Ok(())
}

pub(super) fn context_has_vocalized_copy(context: &Value) -> bool {
    let vocalized = |text: &Value| text.as_str().is_some_and(|text| text.chars().any(arabic_vowel_mark));
    if context.get("title").is_some_and(vocalized) {
        return true;
    }
    // Current-document and continued-book contexts contain bounded source-copy
    // fields. Do not infer editorial policy from skill guidance or model metadata.
    for key in ["objects", "recentApprovedObjects"] {
        if context.get(key).and_then(Value::as_array).is_some_and(|objects| objects.iter().any(|object| object.get("text").is_some_and(vocalized))) {
            return true;
        }
    }
    context.get("approvedPageBriefs").and_then(Value::as_array).is_some_and(|briefs| briefs.iter().any(|brief| brief.get(1).is_some_and(vocalized)))
}

#[cfg(test)]
pub(super) fn simulate(plan: &ProductionPlan, base: &DocState, prefs: &Prefs, mode: OutputMode, cancel: &AtomicBool) -> Result<(), String> {
    simulate_scoped(plan, base, prefs, mode, cancel, None)
}
#[cfg(test)]
pub(super) fn simulate_scoped(
    plan: &ProductionPlan,
    base: &DocState,
    prefs: &Prefs,
    mode: OutputMode,
    cancel: &AtomicBool,
    scope: Option<BatchScope>,
) -> Result<(), String> {
    simulate_styled(plan, base, prefs, mode, cancel, scope, GeometryCheck::Engine)
}

/// Independently verify an ordered partition of one human review on a private draft.
#[cfg(test)]
pub(crate) fn simulate_group_scoped(
    plans: &[ProductionPlan],
    base: &DocState,
    prefs: &Prefs,
    mode: OutputMode,
    cancel: &AtomicBool,
    scope: BatchScope,
) -> Result<(), String> {
    simulate_group_scoped_with_ppi(plans, base, prefs, mode, cancel, scope, None)
}

pub(crate) fn ensure_no_new_low_resolution(preflight: &Value, base: &DocState, required_min_ppi: Option<f64>) -> Result<(), String> {
    let Some(req_ppi) = required_min_ppi else {
        return Ok(());
    };
    let new_low = preflight.get("issues").and_then(Value::as_array).into_iter().flatten().any(|issue| {
        issue.get("kind").and_then(Value::as_str) == Some("lowResolution")
            && issue.get("item").and_then(Value::as_u64).is_some_and(|id| base.doc.item(designcraft_engine::doc::ItemId(id)).is_none())
    });
    if new_low {
        return Err(format!(
            "Generated artwork does not meet the requested {req_ppi} PPI at its actual placement size; no low-resolution pages were published"
        ));
    }
    Ok(())
}

pub(crate) fn simulate_group_scoped_with_ppi(
    plans: &[ProductionPlan],
    base: &DocState,
    prefs: &Prefs,
    mode: OutputMode,
    cancel: &AtomicBool,
    scope: BatchScope,
    required_min_ppi: Option<f64>,
) -> Result<(), String> {
    let min_ppi = required_min_ppi.unwrap_or(150.0);
    let end = review_end(scope)?;
    if plans.is_empty() || plans.len() > MAX_REVIEW_PAGES as usize {
        return Err("Review group must contain 1 to 1000 bounded native plans".into());
    }
    let mut scratch = Session::new();
    scratch.prefs = prefs.clone();
    scratch.add_document(base.clone());
    let mut next = scope.start;
    let mut bytes = 0usize;
    for plan in plans {
        check_cancel(cancel)?;
        if !plan.images.is_empty() || plan.commands.is_empty() || plan.commands.len() > MAX_ACTIONS {
            return Err("Review group plans must contain 1 to 512 native actions and fully expanded images".into());
        }
        bytes = bytes
            .checked_add(serde_json::to_vec(plan).map_err(|e| e.to_string())?.len())
            .filter(|total| *total <= MAX_GROUP_PLAN_BYTES)
            .ok_or("Review plans exceed the 256-MiB limit")?;
        let mut last = None;
        for action in &plan.commands {
            if matches!(action.command.as_str(), "frame.create" | "line.create" | "path.create")
                || (action.command == "object.qrCode" && action.params.get("rect").is_some())
                || (action.command == "text.thread" && action.params.get("rect").is_some())
                || (action.command == "file.place" && action.params.get("frame").is_none())
            {
                let page = action
                    .params
                    .get("spread")
                    .and_then(Value::as_u64)
                    .and_then(|page| u32::try_from(page).ok())
                    .ok_or("Every internal review object needs an explicit native spread")?;
                if page < next || page >= end {
                    return Err("Internal review parts overlap or target a page outside the requested scope".into());
                }
                last = Some(last.map_or(page, |prior: u32| prior.max(page)));
            }
        }
        let chunk_end = last.and_then(|page| page.checked_add(1)).ok_or("Internal review part creates no native page content")?;
        let chunk = BatchScope { start: next, count: chunk_end.checked_sub(next).ok_or("Invalid internal review range")? };
        validate_batch_plan(plan, chunk)?;
        let before = scratch.doc().map_err(|e| e.to_string())?.clone();
        scratch
            .execute(
                "production.apply",
                &json!({"commands":plan.commands,"outputMode":mode,"label":plan.title,"requirePreflight":true,"minPpi":min_ppi}),
            )
            .map_err(|e| e.to_string())?;
        validate_batch_result(&before, scratch.doc().map_err(|e| e.to_string())?, chunk)?;
        next = chunk_end;
    }
    if next != end {
        return Err("Internal review parts do not cover every requested review page".into());
    }
    validate_batch_result(base, scratch.doc().map_err(|e| e.to_string())?, scope)?;
    let preflight = scratch.execute("preflight.run", &json!({"minPpi": min_ppi})).map_err(|e| e.to_string())?;
    if preflight.get("errors").and_then(Value::as_u64).ok_or("Invalid print-preflight response")? > 0 {
        let errors: Vec<_> = preflight
            .get("issues")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|issue| issue.get("severity").and_then(Value::as_str) == Some("error"))
            .take(32)
            .cloned()
            .collect();
        return Err(format!("Print-preflight errors: {}", bounded_json(&json!(errors), 16 * 1024)?));
    }
    ensure_no_new_low_resolution(&preflight, base, required_min_ppi)?;
    check_cancel(cancel)
}

#[cfg(test)]
pub(super) fn simulate_styled(
    plan: &ProductionPlan,
    base: &DocState,
    prefs: &Prefs,
    mode: OutputMode,
    cancel: &AtomicBool,
    scope: Option<BatchScope>,
    geometry: GeometryCheck<'_>,
) -> Result<(), String> {
    simulate_styled_with_ppi(plan, base, prefs, mode, cancel, scope, geometry, None)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn simulate_styled_with_ppi(
    plan: &ProductionPlan,
    base: &DocState,
    prefs: &Prefs,
    mode: OutputMode,
    cancel: &AtomicBool,
    scope: Option<BatchScope>,
    geometry: GeometryCheck<'_>,
    required_min_ppi: Option<f64>,
) -> Result<(), String> {
    let min_ppi = required_min_ppi.unwrap_or(150.0);
    check_cancel(cancel)?;
    if let Some(scope) = scope {
        validate_batch_plan(plan, scope)?;
    }
    let mut scratch = Session::new();
    scratch.prefs = prefs.clone();
    scratch.add_document(base.clone());
    scratch
        .execute("production.apply", &json!({"commands":plan.commands,"outputMode":mode,"label":plan.title,"requirePreflight":true,"minPpi":min_ppi}))
        .map_err(|e| e.to_string())?;
    check_cancel(cancel)?;
    if let Some(scope) = scope {
        validate_batch_result(base, scratch.doc().map_err(|e| e.to_string())?, scope)?;
    }
    match geometry {
        GeometryCheck::Engine => {}
        GeometryCheck::Profile(profile) => crate::design::check_geometry(Some(base), scratch.doc().map_err(|e| e.to_string())?, profile)?,
        GeometryCheck::OpenedDocument { allow_geometry_edits } => {
            check_opened_document_geometry_with_policy(base, scratch.doc().map_err(|e| e.to_string())?, allow_geometry_edits)?
        }
    }
    let preflight = scratch.execute("preflight.run", &json!({"minPpi": min_ppi})).map_err(|e| e.to_string())?;
    if preflight.get("errors").and_then(Value::as_u64).ok_or("Invalid print-preflight response")? > 0 {
        let errors: Vec<_> = preflight
            .get("issues")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|issue| issue.get("severity").and_then(Value::as_str) == Some("error"))
            .take(32)
            .cloned()
            .collect();
        return Err(format!("Print-preflight errors: {}", bounded_json(&json!(errors), 16 * 1024)?));
    }
    ensure_no_new_low_resolution(&preflight, base, required_min_ppi)?;
    check_cancel(cancel)
}

pub(crate) fn validate_batch_plan(plan: &ProductionPlan, scope: BatchScope) -> Result<(), String> {
    let end = scope.start.checked_add(scope.count).filter(|_| scope.count > 0).ok_or("Invalid batch page range")?;
    let mut own_selection = false;
    let mut produced_pages = BTreeSet::new();
    for (index, action) in plan.commands.iter().enumerate() {
        if !batch_command(&action.command) {
            return Err(format!("Command {} cannot modify a page-isolated batch", action.command));
        }
        if action.command == "text.thread" && action.params.get("autoflow").and_then(Value::as_bool) == Some(true) {
            return Err("Command text.thread cannot autoflow pages in a page-isolated batch".into());
        }
        if action.command == "layout.section" {
            let page_1 = action.params.get("page").and_then(Value::as_u64).ok_or("Batch layout.section requires a 1-based page")?;
            let page_0 = page_1.checked_sub(1).ok_or("Batch layout.section page must be >= 1")?;
            if page_0 < u64::from(scope.start) || page_0 >= u64::from(end) {
                return Err("Batch layout.section targets a page outside its approved range".into());
            }
            if action.params.get("remove").and_then(Value::as_bool) == Some(true)
                || action.params.get("startNumber").is_some_and(|v| !v.is_null())
                || action.params.get("style").is_some_and(|v| !v.is_null())
                || action.params.get("prefix").and_then(Value::as_str).is_some_and(|s| !s.is_empty())
                || action.params.get("includePrefix").and_then(Value::as_bool) == Some(true)
            {
                return Err("Batch layout.section may only set marker text within its approved page range".into());
            }
        }
        if action.command == "layout.pages.applyParent" {
            let pages = action
                .params
                .get("pages")
                .and_then(Value::as_array)
                .filter(|items| !items.is_empty())
                .ok_or("Batch layout.pages.applyParent requires non-empty 0-based pages")?;
            for item in pages {
                let page_0 = item.as_u64().ok_or("Batch layout.pages.applyParent page must be a 0-based integer")?;
                if page_0 < u64::from(scope.start) || page_0 >= u64::from(end) {
                    return Err("Batch layout.pages.applyParent targets a page outside its approved range".into());
                }
            }
            let parent_ok = match action.params.get("parent") {
                Some(Value::Null) => true,
                Some(Value::String(name)) => matches!(name.as_str(), "[None]" | "A-Parent" | "A"),
                _ => false,
            };
            if !parent_ok {
                return Err("Batch layout.pages.applyParent may only apply A-Parent or [None]".into());
            }
        }
        if action.command == "transform.move"
            && let Some(target_spread) = action.params.get("toSpread")
        {
            let page = target_spread.as_u64().ok_or("Batch transform.move toSpread must be an integer page index")?;
            if page < u64::from(scope.start) || page >= u64::from(end) {
                return Err("Batch transform.move targets a page outside its approved range".into());
            }
            produced_pages.insert(page);
        }
        if matches!(action.command.as_str(), "frame.create" | "line.create" | "path.create")
            || (action.command == "object.qrCode" && action.params.get("rect").is_some())
            || (action.command == "text.thread" && action.params.get("rect").is_some())
            || (action.command == "file.place" && action.params.get("frame").is_none())
        {
            let page = action.params.get("spread").and_then(Value::as_u64).ok_or("Every batch object needs an explicit native spread")?;
            if page < u64::from(scope.start) || page >= u64::from(end) {
                return Err("Batch object targets a page outside its approved range".into());
            }
            own_selection = true;
            produced_pages.insert(page);
        } else if !own_selection {
            return Err("Batch must establish its own native object before formatting or selecting".into());
        }
        if let Some(params) = action.params.as_object() {
            let keys: &[&str] = if action.command == "text.thread" {
                &["ids", "frame", "story", "table", "captionFrame", "from", "to"]
            } else {
                &["ids", "frame", "story", "table", "captionFrame"]
            };
            for key in keys {
                if let Some(value) = params.get(*key).filter(|v| !v.is_null()) {
                    let refs: Vec<_> = match value {
                        Value::Array(items) => items.iter().collect(),
                        value => vec![value],
                    };
                    for value in refs {
                        let reference = value.as_str().ok_or("Batch object/story identifiers must reference earlier batch results")?;
                        let body = reference
                            .strip_prefix("${")
                            .and_then(|s| s.strip_suffix('}'))
                            .or_else(|| reference.strip_prefix('$'))
                            .ok_or("Literal IDs are forbidden in page-isolated batches")?;
                        let (step, _) = body.split_once('.').ok_or("Invalid batch result reference")?;
                        let step = if step == "last" {
                            index.checked_sub(1).ok_or("Batch result reference must name an earlier action")?
                        } else {
                            step.parse::<usize>().map_err(|_| "Invalid batch result index")?
                        };
                        if step >= index {
                            return Err("Batch result reference must name an earlier action".into());
                        }
                    }
                }
            }
        }
    }
    let start = usize::try_from(scope.start).map_err(|_| "Invalid batch start")?;
    let end = usize::try_from(end).map_err(|_| "Invalid batch end")?;
    if plan.images.iter().any(|image| image.page < start || image.page >= end) {
        return Err("Batch illustration targets a page outside its approved range".into());
    }
    for image in &plan.images {
        produced_pages.insert(u64::try_from(image.page).map_err(|_| "Invalid image page")?);
    }
    if (u64::from(scope.start)..u64::try_from(end).map_err(|_| "Invalid batch end")?).any(|page| !produced_pages.contains(&page)) {
        return Err("Batch must produce native content or an explicit illustration placement on every requested page".into());
    }
    Ok(())
}
pub(crate) fn marker_only_sections_within_scope(
    before: &[designcraft_engine::doc::Section],
    after: &[designcraft_engine::doc::Section],
    start: usize,
    end: usize,
) -> bool {
    let before_map: BTreeMap<usize, _> = before.iter().map(|s| (s.start, s)).collect();
    let after_map: BTreeMap<usize, _> = after.iter().map(|s| (s.start, s)).collect();
    if before_map.len() != before.len() || after_map.len() != after.len() {
        return false;
    }
    for (&sec_start, &old_sec) in &before_map {
        let Some(&new_sec) = after_map.get(&sec_start) else {
            return false;
        };
        if (start..end).contains(&sec_start) {
            let mut normalized = new_sec.clone();
            normalized.marker = old_sec.marker.clone();
            if &normalized != old_sec {
                return false;
            }
        } else if new_sec != old_sec {
            return false;
        }
    }
    for (&sec_start, &new_sec) in &after_map {
        if !before_map.contains_key(&sec_start) {
            if !(start..end).contains(&sec_start) {
                return false;
            }
            if new_sec.start_number.is_some() || new_sec.style != Default::default() || !new_sec.prefix.is_empty() || new_sec.include_prefix {
                return false;
            }
        }
    }
    true
}
pub(crate) fn validate_batch_result(before: &DocState, after: &DocState, scope: BatchScope) -> Result<(), String> {
    let start = usize::try_from(scope.start).map_err(|_| "Invalid batch start")?;
    let end = start.checked_add(usize::try_from(scope.count).map_err(|_| "Invalid batch count")?).ok_or("Invalid batch end")?;
    if end > before.doc.spreads.len() || after.doc.spreads.len() != before.doc.spreads.len() || before.doc.spreads.iter().any(|s| s.pages.len() != 1)
    {
        return Err("Batch cannot add, remove or rearrange document pages".into());
    }
    for (index, (old, new)) in before.doc.spreads.iter().zip(&after.doc.spreads).enumerate() {
        if !(start..end).contains(&index) && old != new {
            return Err("Batch changed an unapproved page".into());
        }
        if (start..end).contains(&index) && !old.items.is_empty() {
            return Err("Batch target pages must remain blank before production".into());
        }
    }
    for (id, story) in &before.doc.stories {
        if after.doc.stories.get(id) != Some(story) {
            return Err("Batch modified an earlier story".into());
        }
    }
    for (id, asset) in &before.doc.assets {
        if after.doc.assets.get(id) != Some(asset) {
            return Err("Batch modified an earlier asset".into());
        }
    }
    let mut normalized = after.doc.as_ref().clone();
    normalized.spreads = before.doc.spreads.clone();
    normalized.stories = before.doc.stories.clone();
    normalized.assets = before.doc.assets.clone();
    normalized.next_id = before.doc.next_id;
    normalized.modified = before.doc.modified;
    if marker_only_sections_within_scope(&before.doc.sections, &normalized.sections, start, end) {
        normalized.sections = before.doc.sections.clone();
    }
    if normalized.swatches.len() >= before.doc.swatches.len()
        && normalized.swatches.get(..before.doc.swatches.len()) == Some(before.doc.swatches.as_slice())
        && normalized.swatches.get(before.doc.swatches.len()..).is_some_and(|extra| {
            extra.iter().all(|s| !s.named && s.hidden && matches!(s.value, designcraft_engine::doc::color::SwatchValue::Gradient { .. }))
        })
    {
        normalized.swatches.truncate(before.doc.swatches.len());
    }
    if &normalized != before.doc.as_ref() {
        return Err("Batch changed global styles, swatches, layers or document settings".into());
    }
    Ok(())
}
#[cfg(test)]
pub(super) fn batch_proofs(
    plan: &ProductionPlan,
    base: &DocState,
    prefs: &Prefs,
    mode: OutputMode,
    cancel: &AtomicBool,
    scope: BatchScope,
) -> Result<Vec<ProofArtifact>, String> {
    check_cancel(cancel)?;
    let mut scratch = Session::new();
    scratch.prefs = prefs.clone();
    scratch.add_document(base.clone());
    scratch
        .execute("production.apply", &json!({"commands":plan.commands,"outputMode":mode,"label":plan.title,"requirePreflight":true,"minPpi":150}))
        .map_err(|e| e.to_string())?;
    batch_proofs_from_draft(&mut scratch, cancel, scope)
}

pub(super) fn push_proof(proofs: &mut Vec<ProofArtifact>, total: &mut usize, name: String, bytes: Vec<u8>) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_PROOF_BYTES {
        return Err("Review proof is empty or exceeds the 256-MiB file limit".into());
    }
    *total = total.checked_add(bytes.len()).filter(|bytes| *bytes <= MAX_PROOFS_BYTES).ok_or("Review proofs exceed the 512-MiB total limit")?;
    proofs.push(ProofArtifact { name, bytes });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn batch_proofs_with_layout(
    plan: &ProductionPlan,
    base: &DocState,
    prefs: &Prefs,
    settings: &AiSettings,
    brief: &str,
    cancel: &AtomicBool,
    scope: BatchScope,
) -> Result<Vec<ProofArtifact>, String> {
    let min_ppi = crate::print_contract::PrintContract::from_prompt(brief)?.and_then(|contract| contract.min_image_ppi()).unwrap_or(150.0);
    let mut scratch = Session::new();
    scratch.prefs = prefs.clone();
    scratch.add_document(base.clone());
    scratch
        .execute("production.apply", &json!({"commands":plan.commands,"outputMode":settings.output_mode,"requirePreflight":true,"minPpi":min_ppi}))
        .map_err(|e| e.to_string())?;
    review_proofs(&mut scratch, cancel, scope, settings, brief)
}
pub(crate) fn render_live_batch_proofs(
    doc: &DocState,
    prefs: &Prefs,
    cache: &Arc<designcraft_engine::compose::Cache>,
    settings: &AiSettings,
    brief: &str,
    scope: BatchScope,
) -> Result<(Vec<ProofArtifact>, Vec<Value>), String> {
    let min_ppi = crate::print_contract::PrintContract::from_prompt(brief)?.and_then(|contract| contract.min_image_ppi()).unwrap_or(150.0);
    let mut scratch = Session::new();
    scratch.cache = cache.clone();
    scratch.prefs = prefs.clone();
    scratch.add_document(doc.clone());
    let preflight = scratch.execute("preflight.run", &json!({"minPpi": min_ppi})).map_err(|e| e.to_string())?;
    let issues = preflight.get("issues").and_then(Value::as_array).cloned().unwrap_or_default();
    let errors: Vec<_> = issues.iter().filter(|issue| issue.get("severity").and_then(Value::as_str) == Some("error")).collect();
    if !errors.is_empty() {
        let summary = errors
            .iter()
            .take(8)
            .map(|issue| {
                let kind = issue.get("kind").and_then(Value::as_str).unwrap_or("preflight");
                let msg = issue.get("message").and_then(Value::as_str).unwrap_or("error");
                format!("{kind}: {msg}")
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("Preflight rejected edited document ({} errors): {summary}", errors.len()));
    }
    let cancel = AtomicBool::new(false);
    let proofs = review_proofs(&mut scratch, &cancel, scope, settings, brief)?;
    Ok((proofs, issues))
}

pub(super) fn review_proofs(
    scratch: &mut Session,
    cancel: &AtomicBool,
    scope: BatchScope,
    settings: &AiSettings,
    brief: &str,
) -> Result<Vec<ProofArtifact>, String> {
    if settings.facing_pages {
        let state = crate::facing_pages::formatted_proof_state(
            scratch.doc().map_err(|e| e.to_string())?,
            &scratch.prefs,
            crate::runtime::book_binding(settings, brief),
        )?;
        let mut proof = Session::new();
        proof.cache = scratch.cache.clone();
        proof.prefs = scratch.prefs.clone();
        proof.add_document(state);
        batch_proofs_from_draft(&mut proof, cancel, scope)
    } else {
        batch_proofs_from_draft(scratch, cancel, scope)
    }
}
pub(super) fn batch_proofs_from_draft(scratch: &mut Session, cancel: &AtomicBool, scope: BatchScope) -> Result<Vec<ProofArtifact>, String> {
    check_cancel(cancel)?;
    let end = review_end(scope)?;
    let state = scratch.doc().map_err(|e| e.to_string())?;
    if usize::try_from(end).map_err(|_| "Invalid review proof range")? > state.doc.page_count() {
        return Err("Review proofs require the exact native absolute-page range".into());
    }
    let count = scope.count.checked_add(scope.count.div_ceil(32)).and_then(|count| count.checked_add(1)).ok_or("Review proof count overflow")?;
    let mut proofs = Vec::with_capacity(usize::try_from(count).map_err(|_| "Invalid review proof count")?);
    let mut total = 0usize;
    let native = designcraft_format::save(&scratch.doc().map_err(|e| e.to_string())?.doc).map_err(|e| e.to_string())?;
    push_proof(&mut proofs, &mut total, "document.designcraft".into(), native)?;
    let mut pdf_start = scope.start;
    while pdf_start < end {
        check_cancel(cancel)?;
        let pdf_end = pdf_start.saturating_add(32).min(end);
        let pages: Vec<_> = (pdf_start..pdf_end).map(|page| page.saturating_add(1)).collect();
        let pdf =
            scratch.execute("file.exportPdf", &json!({"pages":pages,"spreads":false,"bleed":false,"marks":false})).map_err(|e| e.to_string())?;
        let encoded = pdf.get("base64").and_then(Value::as_str).ok_or("Review PDF proof has no bytes")?;
        if encoded.len() > MAX_PROOF_BYTES.saturating_add(2).saturating_div(3).saturating_mul(4) {
            return Err("Review PDF proof exceeds the 256-MiB file limit".into());
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|_| "Invalid review PDF proof encoding")?;
        let name = if scope.count <= 32 { "proof.pdf".into() } else { format!("proof-pages-{}-{pdf_end}.pdf", pdf_start.saturating_add(1)) };
        push_proof(&mut proofs, &mut total, name, bytes)?;
        pdf_start = pdf_end;
    }
    let mut renderer = designcraft_render::Renderer::new();
    for page in scope.start..end {
        check_cancel(cancel)?;
        let state = scratch.doc().map_err(|e| e.to_string())?;
        let native_page = state
            .doc
            .pages()
            .nth(usize::try_from(page).map_err(|_| "Invalid review proof page")?)
            .map(|(_, _, page)| page)
            .ok_or("Review proof page disappeared")?;
        let side = native_page.width.max(native_page.height);
        if !native_page.width.is_finite() || !native_page.height.is_finite() || native_page.width <= 0.0 || native_page.height <= 0.0 {
            return Err("Review proof page has invalid native dimensions".into());
        }
        let scale = (1024.0 / side).min(1.5);
        let image = renderer
            .render_page(
                &state.doc,
                &scratch.cache,
                usize::try_from(page).map_err(|_| "Invalid batch proof page")?,
                scale,
                false,
                &designcraft_render::RenderOptions::default(),
            )
            .ok_or("Cannot render batch proof page")?;
        let bytes = image.to_png();
        if bytes.is_empty() || bytes.len() > 4 * 1024 * 1024 {
            return Err("Batch page proof exceeds its byte limit or is empty".into());
        }
        push_proof(&mut proofs, &mut total, format!("page-{}.png", page.saturating_add(1)), bytes)?;
    }
    if proofs.len() != usize::try_from(count).map_err(|_| "Invalid review proof count")? {
        return Err("Review proofs do not cover the exact requested page range".into());
    }
    check_cancel(cancel)?;
    Ok(proofs)
}
