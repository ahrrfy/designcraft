use super::*;

pub(super) fn skills(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, available: bool, document: bool, requests: &mut Vec<AiRequest>) {
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
        if s.snapshot.stage == "failed" && is_skill_scoped_error(&s.snapshot.message) {
            egui::Frame::group(ui.style()).stroke(egui::Stroke::new(1.0, ui.visuals().error_fg_color)).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    rtl::label(ui, egui::RichText::new(&s.snapshot.message).color(ui.visuals().error_fg_color));
                    if button(ui, a, "Dismiss", "إخفاء") {
                        s.clear_failure_alert();
                    }
                });
            });
        }
        field(ui, a, "Import skill folder or ZIP path", "مسار مجلد المهارة أو ملف ZIP", &mut s.import_path);
        if ui.add_enabled(!s.import_path.trim().is_empty(), egui::Button::new(rtl::widget(ui, text(a, "Import draft", "استيراد مسودة")))).clicked()
        {
            s.import_path = sanitize_user_path(&s.import_path);
            s.clear_failure_alert();
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
