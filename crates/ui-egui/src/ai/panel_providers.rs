use super::*;

pub(super) fn providers(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, available: bool, requests: &mut Vec<AiRequest>) {
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
