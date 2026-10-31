use super::*;

pub(super) fn same_document_path(left: &str, right: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        left.replace('\\', "/").eq_ignore_ascii_case(&right.replace('\\', "/"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        left == right
    }
}

pub(super) fn saved_book_document<'a>(draft: &ProductionDraft, documents: &'a [OpenDocument], selected: Option<usize>) -> Option<&'a OpenDocument> {
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

pub(super) fn preserved_document_geometry<'a>(s: &AiUiState, documents: &'a [OpenDocument], selected: Option<usize>) -> Option<&'a OpenDocument> {
    if s.new_document {
        return None;
    }
    documents.iter().find(|document| Some(document.index) == selected && (!s.continue_book || document.facing_pages))
}

pub(super) fn pending_image_approval_card(ui: &mut egui::Ui, s: &AiUiState, a: bool, requests: &mut Vec<AiRequest>) {
    let Some(approval) = s.snapshot.pending_image_approval.clone() else {
        return;
    };
    let provider_name = match approval.provider {
        ProviderKind::Gemini => "Gemini",
        ProviderKind::OpenAi => "OpenAI",
        ProviderKind::Ollama => "Ollama",
        ProviderKind::ComfyUi => "ComfyUI",
    };
    egui::Frame::group(ui.style()).show(ui, |ui| {
        let heading = if a {
            format!("تأكيد إعادة توليد {} صورة ({provider_name}: {})", approval.count, approval.model)
        } else {
            format!("Confirm regenerating {} illustration(s) ({provider_name}: {})", approval.count, approval.model)
        };
        rtl::label(ui, egui::RichText::new(&heading).strong());
        if !approval.summary.is_empty() {
            rtl::label(ui, &approval.summary);
        }
        let is_local_provider = matches!(approval.provider, ProviderKind::ComfyUi | ProviderKind::Ollama);
        let cost_note = if is_local_provider {
            if a {
                "توليد محلي مجاني ($0.00) · تُطبَّق الصور في خطوة تراجع واحدة.".to_owned()
            } else {
                "Free local image generation ($0.00) · applies in a single undo step.".to_owned()
            }
        } else if let Some(cost) = approval.estimated_cost_usd {
            if a {
                format!("التكلفة التقديرية لطلبات الصور السحابية: ${cost:.4} · تُطبَّق في خطوة تراجع واحدة.")
            } else {
                format!("Estimated paid cloud image cost: ${cost:.4} · applies in a single undo step.")
            }
        } else if a {
            "تتطلب الصور السحابية المدفوعة تأكيدك قبل الصرف · تُطبَّق في خطوة تراجع واحدة.".to_owned()
        } else {
            "Paid cloud image generation requires your confirmation before spending quota · applies in a single undo step.".to_owned()
        };
        rtl::label(ui, &cost_note);
        ui.horizontal_wrapped(|ui| {
            let confirm_label = if a {
                format!("تأكيد وتوليد {} صورة الآن", approval.count)
            } else {
                format!("Confirm & generate {} image(s)", approval.count)
            };
            if ui.add_enabled(!s.snapshot.busy, egui::Button::new(rtl::widget(ui, &confirm_label))).clicked() {
                requests.push(AiRequest::ConfirmImageRegeneration { job_id: approval.job_id.clone() });
            }
            let dismiss_label = text(a, "Keep layout edits without regenerating images", "الإبقاء على تعديلات النص والتصميم دون توليد الصور");
            if ui.add_enabled(!s.snapshot.busy, egui::Button::new(rtl::widget(ui, dismiss_label))).clicked() {
                requests.push(AiRequest::DismissImageRegeneration { job_id: approval.job_id });
            }
        });
    });
}

pub(super) fn arabic_typography_bar(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, requests: &mut Vec<AiRequest>) {
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
    });
    if settings_changed {
        requests.push(AiRequest::UpdateSettings(s.settings.clone()));
    }
}

pub(super) fn production_binding(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, documents: &[OpenDocument], selected: Option<usize>) {
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

pub(super) fn matching_retryable_failed_batch<'a>(
    s: &'a AiUiState,
    documents: &[OpenDocument],
    selected: Option<usize>,
) -> Option<&'a BatchSnapshot> {
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

pub(super) fn retryable_consumed_usage(s: &AiUiState, documents: &[OpenDocument], selected: Option<usize>) -> (u32, u64) {
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
pub(super) struct BudgetTargets {
    pub(super) consumed_requests: u32,
    pub(super) book_requests: u32,
    pub(super) review_requests: u32,
    pub(super) min_text_requests: u32,
    pub(super) book_tokens: Option<u64>,
    pub(super) review_tokens: Option<u64>,
}

pub(super) fn budget_targets_for_estimate(
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

    let book_requests = if active_batch.is_some() {
        consumed_requests.saturating_add(retry_remaining_pages.saturating_mul(per_page_requests)).max(estimate.book_allowance)
    } else {
        estimate.book_allowance
    };
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

pub(super) fn budget_targets(s: &AiUiState, documents: &[OpenDocument], selected: Option<usize>) -> Option<BudgetTargets> {
    let estimate = s.request_estimate(documents, selected)?;
    let tokens = s.token_estimate(documents, selected);
    Some(budget_targets_for_estimate(s, documents, selected, &estimate, tokens))
}

pub(super) fn production_recovery(
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

pub(super) fn render_diagnostic_alert_banner(
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

pub(super) fn selectable_skill_cards(skills: &[SkillCard], include_design_profiles: bool, require_trusted_or_active: bool) -> Vec<&SkillCard> {
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

pub(super) fn production_blocking_issue(
    s: &AiUiState,
    available: bool,
    documents: &[OpenDocument],
    active: Option<usize>,
    a: bool,
) -> Option<&'static str> {
    let explicit_pages = s.analyzed_prompt().explicit_pages;
    let requested_pages = s.requested_target_pages();
    let has_prompt = !s.prompt.trim().is_empty();
    let script_conflict_issue = explicit_pages.and_then(|script_pages| {
        let mismatch = if s.new_document {
            s.pages != script_pages
        } else if s.continue_book {
            documents.iter().find(|document| Some(document.index) == active).is_some_and(|document| {
                u32::try_from(document.pages).ok().is_none_or(|cur| cur > script_pages || (cur < script_pages && document.has_interior_gaps()))
            })
        } else {
            false
        };
        mismatch.then_some(text(
            a,
            "Explicit PAGE script count must match the target book page count.",
            "يجب أن يطابق عدد صفحات التعليمات الصريحة (PAGE) عدد صفحات الكتاب المستهدف.",
        ))
    });
    let continuation_issue = (s.continue_book
        && !documents.iter().any(|document| Some(document.index) == active && document.can_continue_with_target(requested_pages, has_prompt)))
    .then_some(text(
        a,
        "Select a book with completed opening pages and remaining blank pages, or choose Edit current document.",
        "اختر كتاباً فيه صفحات أولى مكتملة ثم صفحات فارغة، أو اختر تحرير المستند الحالي.",
    ));
    s.production_issue(available, active.is_some(), a).or(continuation_issue).or(script_conflict_issue)
}

pub(super) fn production(
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
            ui.horizontal_wrapped(|ui| {
                label(ui, a, "Visual density", "كثافة الرسوم والصور");
                let prev_density = s.settings.visual_density;
                for (mode, en, ar) in [
                    (
                        VisualDensityMode::SmartHybrid,
                        "Smart hybrid (seasonal motifs + vector)",
                        "الهجين الذكي (زخارف شهرية + فيكتور أصيل)",
                    ),
                    (
                        VisualDensityMode::PureNativeVector,
                        "100% pure native vector (0 image calls)",
                        "فيكتور أصيل 100% (سرعة قصوى بدون طلبات صور)",
                    ),
                    (
                        VisualDensityMode::RichPerPageImages,
                        "Rich custom illustrations per page",
                        "رسوم مخصصة غنية لكل صفحة",
                    ),
                ] {
                    ui.selectable_value(&mut s.settings.visual_density, mode, rtl::widget(ui, text(a, en, ar)));
                }
                if s.settings.visual_density != prev_density {
                    s.rates_confirmed = has_configured_rates(&s.settings);
                    if s.workflow_ready() {
                        requests.push(AiRequest::UpdateSettings(s.settings.clone()));
                    }
                }
            });
            if s.settings.visual_density != VisualDensityMode::PureNativeVector {
                model(ui, a, "production_image_provider", &mut s.settings.image_model, true, &mut s.pending_models, &s.discovered_models, requests);
                image_inspection_setting(ui, s, a);
            }
        }
        ui.horizontal_wrapped(|ui| {
            label(ui, a, "What would you like to create?", "ما الذي تريد إنتاجه؟");
            if button(ui, a, "Load script file (.md/.txt)…", "تحميل ملف سكربت (.md/.txt)…") {
                s.pick_script_file = true;
            }
            if !s.prompt.is_empty() && button(ui, a, "Clear prompt", "تفريغ النص") {
                s.prompt.clear();
                s.edit_large_script_inline = false;
                if s.snapshot.batch.as_ref().is_none_or(|b| !b.retryable) {
                    s.clear_failure_alert();
                }
            }
        });
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
        // briefs > 16 KiB render a zero-freeze summary card unless raw editing is toggled.
        let (cached_has_rtl, cached_explicit_pages, cached_natural_pages, cached_preview_head) = {
            let analysis = s.analyzed_prompt();
            (analysis.has_rtl, analysis.explicit_pages, analysis.natural_pages, analysis.preview_head.clone())
        };
        if s.prompt.len() > 16_384 && !s.edit_large_script_inline {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                let kb = s.prompt.len().div_ceil(1024);
                let detected_summary = if let Some(pages) = cached_explicit_pages {
                    if a {
                        format!("تم تحميل سكربت إنتاج كبير ({kb} كيلوبايت · {pages} صفحة صريحة) — التمرير السريع وتقسيم الدفعات مفعلان تلقائياً.")
                    } else {
                        format!("Large production script loaded ({kb} KiB · {pages} explicit pages) — zero-lag mode and per-batch slicing active.")
                    }
                } else if let Some(pages) = cached_natural_pages {
                    if a {
                        format!("تم تحميل وصف إنتاج كبير ({kb} كيلوبايت · {pages} صفحة مكتشفة) — الوضع السريع مفعل.")
                    } else {
                        format!("Large production brief loaded ({kb} KiB · {pages} pages detected) — zero-lag mode active.")
                    }
                } else if a {
                    format!("تم تحميل نص إنتاج كبير ({kb} كيلوبايت) — الوضع السريع مفعل لمنع تجميد الواجهة.")
                } else {
                    format!("Large production brief loaded ({kb} KiB) — zero-lag mode active.")
                };
                rtl::label(ui, egui::RichText::new(detected_summary).strong());
                if !cached_preview_head.is_empty() {
                    rtl::label(ui, egui::RichText::new(&cached_preview_head).weak());
                }
                ui.horizontal_wrapped(|ui| {
                    if button(ui, a, "Edit raw script text", "تحرير النص الخام") {
                        s.edit_large_script_inline = true;
                    }
                    if button(ui, a, "Replace script file…", "استبدال ملف السكربت…") {
                        s.pick_script_file = true;
                    }
                });
            });
        } else {
            if s.prompt.len() > 16_384 {
                ui.horizontal_wrapped(|ui| {
                    if button(ui, a, "Collapse to fast script card", "العودة لبطاقة السكربت السريعة") {
                        s.edit_large_script_inline = false;
                    }
                });
                let prev_len = s.prompt.len();
                egui::ScrollArea::vertical().id_salt("ai_prompt_composer_raw").max_height(110.0).show(ui, |ui| {
                    ui.add(egui::TextEdit::multiline(&mut s.prompt).code_editor().desired_rows(5).desired_width(f32::INFINITY));
                });
                if s.prompt.len() != prev_len && s.snapshot.batch.as_ref().is_none_or(|b| !b.retryable) {
                    s.clear_failure_alert();
                }
            } else {
                let prompt_rtl = if s.prompt.trim().is_empty() { a } else { cached_has_rtl };
                let prev_prompt_fp = PromptAnalysisCache::fingerprint(&s.prompt);
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
                if PromptAnalysisCache::fingerprint(&s.prompt) != prev_prompt_fp
                    && s.snapshot.batch.as_ref().is_none_or(|b| !b.retryable)
                {
                    s.clear_failure_alert();
                }
            }
        }
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
            if cached_explicit_pages.is_some() {
                label(
                    ui,
                    a,
                    "Explicit PAGE script is sliced automatically per batch; only the active batch pages and compact style header are sent per request.",
                    "يُقسّم السكربت الصريح (PAGE) تلقائياً لكل دفعة؛ يُرسل محتوى صفحات الدفعة الحالية وترويسة النمط المضغوطة فقط في كل طلب.",
                );
            } else {
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
            let pacing_label = match s.settings.execution_pacing {
                ExecutionPacing::AutoContinue => text(a, "Auto-continue full book", "إنتاج مستمر تلقائياً للكتاب بالكامل"),
                ExecutionPacing::StopAfterFirstBatch => {
                    text(a, "Review 1st batch, then auto-continue", "مراجعة الدفعة الأولى فقط ثم إكمال تلقائي")
                }
                ExecutionPacing::ReviewEveryBatch => text(a, "Pause every batch for proof", "التوقف للمراجعة بعد كل دفعة"),
            };
            let prev_pacing = s.settings.execution_pacing;
            egui::ComboBox::from_id_salt("ai_execution_pacing").selected_text(rtl::widget(ui, pacing_label)).show_ui(ui, |ui| {
                for (mode, en, ar) in [
                    (
                        ExecutionPacing::AutoContinue,
                        "Auto-continue full book",
                        "إنتاج مستمر تلقائياً للكتاب بالكامل",
                    ),
                    (
                        ExecutionPacing::StopAfterFirstBatch,
                        "Review 1st batch, then auto-continue",
                        "مراجعة الدفعة الأولى فقط ثم إكمال تلقائي",
                    ),
                    (
                        ExecutionPacing::ReviewEveryBatch,
                        "Pause every batch for proof",
                        "التوقف للمراجعة بعد كل دفعة",
                    ),
                ] {
                    ui.selectable_value(&mut s.settings.execution_pacing, mode, rtl::widget(ui, text(a, en, ar)));
                }
            });
            if s.settings.execution_pacing != prev_pacing && s.workflow_ready() {
                requests.push(AiRequest::UpdateSettings(s.settings.clone()));
            }
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
                && let Some(reason) = selected.continuation_guidance(a, s.requested_target_pages(), !s.prompt.trim().is_empty())
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
        if let Some(script_pages) = cached_explicit_pages {
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
                && !(s.continue_book
                    && !selected.has_interior_gaps()
                    && u32::try_from(selected.pages).ok().is_some_and(|cur| cur < script_pages))
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
            && let Some(detected) = cached_natural_pages
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
        match s.settings.execution_pacing {
            ExecutionPacing::AutoContinue => label(
                ui,
                a,
                "Continuous production: batches publish live to the document and continue automatically until the book finishes (or you click Pause).",
                "توليد مستمر تلقائياً: تُنشر كل دفعة مباشرة في المستند وتستمر الدفعات تلقائياً حتى اكتمال الكتاب (أو الضغط على إيقاف مؤقت).",
            ),
            ExecutionPacing::StopAfterFirstBatch => label(
                ui,
                a,
                "First-batch proof: stops once after batch 1 for your visual review, then continues the remaining book automatically.",
                "اعتماد الدفعة الأولى فقط: يتوقف مرة واحدة بعد الدفعة الأولى لمراجعتك البصرية، ثم يكمل بقية الكتاب تلقائياً.",
            ),
            ExecutionPacing::ReviewEveryBatch => label(
                ui,
                a,
                "Your review page count stays unchanged. Internal requests build this group, then stop for your approval before the next review group.",
                "يبقى عدد صفحات مراجعتك كما اخترته. تُنفّذ المجموعة بطلبات داخلية، ثم تتوقف لاعتمادك قبل مجموعة المراجعة التالية.",
            ),
        }
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
        pending_image_approval_card(ui, s, a, requests);
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
        arabic_typography_bar(ui, s, a, requests);
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

pub(super) fn elapsed(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{:02}:{:02}:{:02}", seconds / 3600, (seconds / 60) % 60, seconds % 60)
}

pub(super) fn usage_line(ui: &mut egui::Ui, a: bool, usage: &designcraft_ai_types::protocol::UsageSummary, elapsed_ms: u64) {
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

pub(super) fn proof_image(
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

pub(super) fn batch_review(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, requests: &mut Vec<AiRequest>) {
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

pub(super) fn image_inspection_setting(ui: &mut egui::Ui, s: &mut AiUiState, a: bool) {
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

pub(super) fn budgets(ui: &mut egui::Ui, s: &mut AiUiState, a: bool) {
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
pub(super) fn optional_rate(ui: &mut egui::Ui, a: bool, en: &str, ar: &str, rate: &mut Option<f64>) {
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
pub(super) fn export_print(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, enabled: bool, requests: &mut Vec<AiRequest>) {
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
