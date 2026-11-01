use super::{AiUiState, label, provider_name, rtl, text};

pub(super) fn usage(ui: &mut egui::Ui, s: &AiUiState, a: bool) {
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
