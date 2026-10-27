use super::*;

pub(super) fn local_ai(ui: &mut egui::Ui, s: &mut AiUiState, a: bool, available: bool, can_pick: bool, requests: &mut Vec<AiRequest>) {
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
