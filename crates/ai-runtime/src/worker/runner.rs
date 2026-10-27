use super::*;

pub(crate) struct Runner<'a> {
    pub(super) cache_root: Option<PathBuf>,
    pub(super) print: Option<crate::print_contract::PrintContract>,
    pub(super) planning_only: bool,
    pub(super) ledger: &'a mut UsageLedger,
    pub(super) keys: &'a [SessionKey],
    pub(super) cancel: &'a AtomicBool,
    pub(super) sender: &'a Sender<Message>,
    pub(super) provider: &'a dyn Provider,
}
impl Runner<'_> {
    pub(super) fn image_disk_key(
        &self,
        placement: &designcraft_ai_types::protocol::ImagePlacement,
        style: &str,
        profile: Option<&designcraft_ai_types::DesignProfile>,
        settings: &AiSettings,
    ) -> Result<String, String> {
        crate::production_cache::digest(
            &json!({"version":"ai18-illustration-v2","placement":placement,"style":style,"model":settings.image_model,"workflow":settings.comfy_workflow,"review":image_review_identity(settings),"size":self.print_image_size(placement,profile,&settings.image_model)?}),
        )
    }
    pub(super) fn preload_images(
        &self,
        plan: &ProductionPlan,
        settings: &AiSettings,
        profile: Option<&designcraft_ai_types::DesignProfile>,
        style: &str,
        cache: &mut BTreeMap<String, providers::GeneratedImage>,
        bytes: &mut usize,
    ) -> Result<(), String> {
        let Some(root) = &self.cache_root else {
            return Ok(());
        };
        for placement in &plan.images {
            let key = image_memory_key(placement, style, settings)?;
            if cache.contains_key(&key) {
                continue;
            }
            let disk_key = self.image_disk_key(placement, style, profile, settings)?;
            match crate::production_cache::load_image(root, &disk_key) {
                Ok(Some(image)) => {
                    validate_image(&image)?;
                    *bytes = bytes
                        .checked_add(image.bytes.len())
                        .filter(|n| *n <= MAX_UNIQUE_RASTER_BYTES)
                        .ok_or("Cached unique image sources exceed this plan's 32-MiB raster budget")?;
                    cache.insert(key, image);
                    self.cache_notice(format!("Reused page {} artwork locally; no new image request was billed", placement.page.saturating_add(1)))?;
                    if settings.verify_generated_images {
                        self.diagnostics(vec![json!({"kind":"illustrationSemanticReview","severity":"warning","page":placement.page.saturating_add(1),"cached":true,"message":"Locally reviewed artwork was reused without another billed review. Its previous visual verdict was advisory; human proof remains required."})])?;
                    }
                }
                Ok(None) => {}
                Err(error) => self.cache_notice(format!("Ignored invalid cached illustration: {error}"))?,
            }
        }
        Ok(())
    }
    pub(super) fn cache_notice(&self, message: String) -> Result<(), String> {
        self.diagnostics(vec![json!({"kind":"localProductionCache","message":message})])
    }
    pub(super) fn plan_key(&self, request: &TaskRequest, generation: &GenerationRequest, base: &DocState) -> Result<Option<String>, String> {
        if self.cache_root.is_none() {
            return Ok(None);
        }
        let mut prompt: Value = serde_json::from_str(&generation.prompt).map_err(|e| e.to_string())?;
        if matches!(request.target, designcraft_ai_types::protocol::TaskTarget::NewA4 { .. }) {
            fn strip(value: &mut Value, depth: u32) {
                if depth > 64 {
                    return;
                }
                match value {
                    Value::Object(object) => {
                        for key in ["documentUid", "revision", "jobId"] {
                            object.remove(key);
                        }
                        for child in object.values_mut() {
                            strip(child, depth + 1);
                        }
                    }
                    Value::Array(array) => {
                        for child in array {
                            strip(child, depth + 1);
                        }
                    }
                    _ => {}
                }
            }
            strip(&mut prompt, 0);
        }
        crate::production_cache::digest(&json!({"version":"ai18-native-contract-v1","model":request.settings.text_model,"output":request.settings.output_mode,"arabicTashkeel":request.settings.arabic_tashkeel,"arabicFont":request.settings.arabic_font,"system":generation.system,"schema":generation.json_schema,"prompt":prompt,"outputLimit":generation.max_output_tokens,"geometry":base.doc.settings,"nextNativeId":base.doc.next_id})).map(Some)
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) fn cached_plan(
        &self,
        key: Option<&str>,
        request: &TaskRequest,
        context: &Value,
        base: &DocState,
        prefs: &Prefs,
        scope: Option<BatchScope>,
        catalog: &Value,
        profile: Option<&designcraft_ai_types::DesignProfile>,
    ) -> Result<Option<ProductionPlan>, String> {
        let (Some(root), Some(key)) = (&self.cache_root, key) else {
            return Ok(None);
        };
        let cached = match crate::production_cache::load_plan(root, key) {
            Ok(value) => value,
            Err(error) => {
                self.cache_notice(format!("Ignored invalid cached plan: {error}"))?;
                return Ok(None);
            }
        };
        let Some(mut plan) = cached else {
            return Ok(None);
        };
        let valid = (|| {
            crate::native_layout::normalize_command_aliases(&mut plan);
            validate_actions(&plan.commands, catalog, OutputMode::NativeOnly)?;
            prepare_editorial_plan_with_mode(&mut plan, &request.prompt, context, None, request.settings.arabic_tashkeel)?;
            if let Some(scope) = scope {
                validate_batch_plan(&plan, scope)?;
                crate::script_contract::validate_plan(&request.prompt, scope, &plan)?;
            }
            if let Some(contract) = &self.print {
                self.diagnostics(crate::native_print_fonts::apply(&mut plan, base, prefs, contract)?)?;
            } else if let Some(explicit_contract) = explicit_arabic_font_contract(base, request.settings.arabic_font.as_deref(), &request.prompt) {
                self.diagnostics(crate::native_print_fonts::apply(&mut plan, base, prefs, &explicit_contract)?)?;
            }
            self.diagnostics(crate::native_layout::prepare(&mut plan, base, prefs, self.cancel)?)?;
            validate_previous_body_layout(&plan, base, prefs, scope, &request.prompt)?;
            image_command_budget(&plan, profile)?;
            Ok::<_, String>(())
        })();
        if let Err(error) = valid {
            check_cancel(self.cancel)?;
            self.cache_notice(format!("Cached plan no longer fits this document: {error}"))?;
            return Ok(None);
        }
        self.cache_notice("Reused validated page content locally; no new planning request was billed".into())?;
        Ok(Some(plan))
    }
    pub(super) fn cache_plan(&self, key: Option<&str>, plan: &ProductionPlan) -> Result<(), String> {
        if let (Some(root), Some(key)) = (&self.cache_root, key)
            && let Err(error) = crate::production_cache::store_plan(root, key, plan)
        {
            self.cache_notice(format!("Could not cache this validated plan: {error}"))?;
        }
        Ok(())
    }
    pub(super) fn print_image_size(
        &self,
        placement: &designcraft_ai_types::protocol::ImagePlacement,
        profile: Option<&designcraft_ai_types::DesignProfile>,
        selection: &ModelSelection,
    ) -> Result<Option<String>, String> {
        let original = image_request_size(placement, profile, selection)?;
        let Some(required) = self.required_source_pixels(placement, profile)? else {
            return Ok(original);
        };
        if selection.provider == ProviderKind::Gemini {
            let ratio = original.as_deref().ok_or("Gemini print image has no aspect ratio")?;
            return providers::gemini_print_size(&selection.model, ratio, required).map(Some);
        }
        Ok(original)
    }
    pub(super) fn required_source_pixels(
        &self,
        placement: &designcraft_ai_types::protocol::ImagePlacement,
        profile: Option<&designcraft_ai_types::DesignProfile>,
    ) -> Result<Option<[u32; 2]>, String> {
        let Some(contract) = self.print.as_ref() else { return Ok(None) };
        let Some(mut required) = contract.required_image_pixels(placement.rect)? else { return Ok(None) };
        let slot = declared_image_slot(placement, profile);
        let mut crops = vec![(placement.rect, slot.and_then(|slot| slot.crop))];
        if let Some(slot) = slot {
            for view in &slot.views {
                crops.push((resolved_slot_view_rect(placement, slot, profile, view.rect), Some(view.crop)));
            }
        } else {
            crops.extend(placement.repeat_rects.iter().map(|rect| (*rect, None)));
        }
        for (rect, crop) in crops {
            if let Some(pixels) = contract.required_image_pixels(rect)? {
                let [x0, y0, x1, y1] = crop.unwrap_or([0.0, 0.0, 1.0, 1.0]);
                let width = x1 - x0;
                let height = y1 - y0;
                if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                    return Err("Invalid print image crop".into());
                }
                let w = (f64::from(pixels[0]) / width).ceil();
                let h = (f64::from(pixels[1]) / height).ceil();
                if w > 16384.0 || h > 16384.0 {
                    return Err("Requested image crop exceeds supported print resolution before generation".into());
                }
                required[0] = required[0].max(w as u32);
                required[1] = required[1].max(h as u32);
            }
        }
        Ok(Some(required))
    }
    /// Provider outputs are alternatives for one native placement. Quality here
    /// means bounded decodability, geometry and resolution, never semantic approval.
    pub(super) fn choose_placement_image(
        &self,
        images: Vec<providers::GeneratedImage>,
        placement: &designcraft_ai_types::protocol::ImagePlacement,
        profile: Option<&designcraft_ai_types::DesignProfile>,
    ) -> Result<providers::GeneratedImage, String> {
        if images.is_empty() || images.len() > MAX_IMAGES {
            return Err(format!(
                "Image provider returned {} final images for page {}; expected 1–{MAX_IMAGES} bounded alternatives",
                images.len(),
                placement.page.saturating_add(1)
            ));
        }
        let count = images.len();
        let slot = declared_image_slot(placement, profile);
        let ratio = image_source_ratio(placement, slot)?;
        let required = self.required_source_pixels(placement, profile)?;
        let mut best: Option<(usize, f64, u64, providers::GeneratedImage)> = None;
        let mut candidates = Vec::with_capacity(count);
        let mut normalized_bytes = 0usize;
        for (index, raw) in images.into_iter().enumerate() {
            check_cancel(self.cancel)?;
            let checked = (|| -> Result<(providers::GeneratedImage, u32, u32), String> {
                let image = prepare_generated_image(raw)?;
                normalized_bytes = normalized_bytes
                    .checked_add(image.bytes.len())
                    .filter(|total| *total <= MAX_UNIQUE_RASTER_BYTES)
                    .ok_or("Normalized image alternatives exceed the 32-MiB response budget")?;
                let mut reader =
                    image::ImageReader::new(std::io::Cursor::new(&image.bytes)).with_guessed_format().map_err(|error| error.to_string())?;
                let mut limits = image::Limits::default();
                limits.max_alloc = Some(128 * 1024 * 1024);
                limits.max_image_width = Some(16384);
                limits.max_image_height = Some(16384);
                reader.limits(limits);
                let decoded = reader.decode().map_err(|error| format!("Invalid image alternative: {error}"))?;
                let (width, height) = (decoded.width(), decoded.height());
                validate_image_dimensions(width, height)?;
                Ok((image, width, height))
            })();
            let (image, width, height) = match checked {
                Ok(value) => value,
                Err(error) => {
                    candidates.push(json!({"index":index,"eligible":false,"reason":error}));
                    continue;
                }
            };
            let digest = format!("{:x}", Sha256::digest(&image.bytes));
            let mut evidence = json!({"index":index,"width":width,"height":height,"sha256":digest,"eligible":false});
            if count > 1
                && let Some(root) = &self.cache_root
            {
                let key = crate::production_cache::digest(&json!({"policy":"ai24-image-alternative-v1","placement":placement,"sha256":digest}))?;
                match crate::production_cache::store_image(root, &key, &image) {
                    Ok(()) => evidence["alternative_cache_key"] = json!(key),
                    Err(error) => evidence["cache_warning"] = json!(error),
                }
            }
            let actual = f64::from(width) / f64::from(height);
            let reason = if required.is_some_and(|[w, h]| width < w || height < h) {
                Some("Below the requested print resolution")
            } else if slot.filter(|slot| !slot.views.is_empty()).is_some() && (actual / ratio - 1.0).abs() > 0.08 {
                Some("Incompatible with the declared shared-image crop geometry")
            } else {
                None
            };
            if let Some(reason) = reason {
                evidence["reason"] = json!(reason);
            } else {
                let loss = 1.0 - (actual / ratio).min(ratio / actual);
                let pixels = u64::from(width) * u64::from(height);
                evidence["eligible"] = json!(true);
                evidence["crop_loss"] = json!(loss);
                if best.as_ref().is_none_or(|(_, old_loss, old_pixels, _)| loss < *old_loss || (loss == *old_loss && pixels > *old_pixels)) {
                    best = Some((index, loss, pixels, image));
                }
            }
            candidates.push(evidence);
        }
        let selected = best.as_ref().map(|(index, _, _, _)| *index);
        if count > 1 || selected.is_none() {
            self.diagnostics(vec![json!({"kind":"imageOutputSelection","page":placement.page.saturating_add(1),"final_images":count,
                "selected_index":selected,"policy":"valid printable image, minimum crop loss, higher pixel count, then provider order",
                "candidates":candidates,"human_review_required":true,"semantic_content_verified":false,
                "message":"One alternative fills one editable native frame; provider usage includes every output. No additional generation request was made for selection."})])?;
        }
        check_cancel(self.cancel)?;
        best.map(|(_, _, _, image)| image).ok_or_else(|| {
            format!(
                "No valid final image meets geometry and print resolution for page {}; inspect imageOutputSelection diagnostics",
                placement.page.saturating_add(1)
            )
        })
    }
    pub(super) fn audit_print(&self, plan: &ProductionPlan, base: &DocState, prefs: &Prefs) -> Result<(), String> {
        let min_ppi = self.print.as_ref().and_then(|contract| contract.min_image_ppi()).unwrap_or(150.0);
        let mut scratch = Session::new();
        scratch.prefs = prefs.clone();
        scratch.add_document(base.clone());
        scratch
            .execute(
                "production.apply",
                &json!({"commands":plan.commands,"outputMode":"native_with_images","requirePreflight":true,"minPpi":min_ppi}),
            )
            .map_err(|e| e.to_string())?;
        let result = scratch.execute("preflight.run", &json!({"minPpi":min_ppi})).map_err(|e| e.to_string())?;
        let issues: Vec<Value> = result.get("issues").and_then(Value::as_array).into_iter().flatten().take(256).cloned().collect();
        let new_low = issues.iter().any(|issue| {
            issue.get("kind").and_then(Value::as_str) == Some("lowResolution")
                && issue.get("item").and_then(Value::as_u64).is_some_and(|id| base.doc.item(designcraft_engine::doc::ItemId(id)).is_none())
        });
        self.diagnostics(issues)?;
        if self.print.as_ref().and_then(|contract| contract.min_image_ppi()).is_some() && new_low {
            return Err(format!(
                "Generated artwork does not meet the requested {min_ppi} PPI at its actual placement size; no low-resolution pages were published"
            ));
        }
        Ok(())
    }
    pub(super) fn usage(&self) -> Result<(), String> {
        self.sender.send(Message::Usage(self.ledger.summary())).map_err(|_| "AI host disconnected".to_string())
    }
    pub(super) fn diagnostics(&self, issues: Vec<Value>) -> Result<(), String> {
        for issue in issues {
            self.sender.send(Message::Diagnostic(issue)).map_err(|_| "AI host disconnected".to_string())?;
        }
        Ok(())
    }
    pub(crate) fn progress(&self, stage: &str, fraction: f32) -> Result<(), String> {
        check_cancel(self.cancel)?;
        self.usage()?;
        self.sender.send(Message::Progress { stage: stage.into(), fraction }).map_err(|_| "AI host disconnected".to_string())
    }
    pub(crate) fn remaining_requests(&self) -> u32 {
        self.ledger.remaining_requests()
    }
    pub(crate) fn required_min_ppi(&self) -> Option<f64> {
        self.print.as_ref().and_then(|contract| contract.min_image_ppi())
    }
    pub(super) fn run(&mut self, work: Work) -> Result<Product, String> {
        match work {
            Work::Indesign(export) => {
                self.progress("Converting native IDML through installed InDesign", 0.1)?;
                export.run_with_cancel(std::time::Duration::from_secs(180), self.cancel).map(Product::Indesign)
            }
            Work::Resources => {
                self.progress("Reading host CPU, RAM and available GPU telemetry", 0.1)?;
                Ok(Product::Resources(crate::resources::probe_host()))
            }
            Work::Production { request, context, catalog, skills, base, prefs } => self.production(request, context, catalog, skills, base, prefs),
            Work::DeferredImages { title, batches, settings, style_profile, cache_root, min_ppi, base, prefs } => {
                self.cache_root = cache_root;
                let total_imgs: usize = batches.iter().map(|b| b.images.len()).sum();
                self.progress(&format!("Generating {total_imgs} confirmed single-subject illustration(s)"), 0.15)?;
                let mut scratch = Session::new();
                scratch.prefs = prefs;
                scratch.add_document(base);
                let chunks = self.expand_deferred_image_batches(&title, &batches, &settings, style_profile.as_ref(), &mut scratch, min_ppi)?;
                Ok(Product::Agentic {
                    title,
                    summary: format!("Generated and placed {total_imgs} confirmed illustration(s) in 1 Undo step"),
                    chunks,
                    deferred_images: Vec::new(),
                })
            }
            Work::Skill { prompt, reference_paths, context, settings, catalog, registry_root, design_style } => {
                self.skill(prompt, reference_paths, context, settings, catalog, registry_root, design_style)
            }
            Work::Models(selection) => {
                self.progress("Discovering selected provider models", 0.1)?;
                let key = selected_key(self.keys, &selection);
                let reservation = self.ledger.reserve_with_output(0, 0, selection.provider, false, None, Some(0))?;
                let result = guarded(|| {
                    self.provider
                        .model_infos(&selection, key.as_ref().map(providers::SecretString::expose), self.cancel)
                        .map(|models| filter_discovered_models(selection.provider, models))
                });
                // Discovery has no token usage field; do not invent a reported zero.
                self.ledger.record(reservation, None);
                check_cancel(self.cancel)?;
                result.map(Product::Models)
            }
        }
    }
    pub(crate) fn agent_turn(
        &mut self,
        selection: &ModelSelection,
        request: &providers::AgentTurnRequest,
    ) -> Result<providers::AgentTurnResponse, String> {
        check_cancel(self.cancel)?;
        let mut msg_bytes = 0usize;
        let mut image_bytes = 0usize;
        let mut image_count = 0usize;
        for m in &request.messages {
            match m {
                providers::AgentMessage::User { text, images } => {
                    msg_bytes = msg_bytes.saturating_add(text.len());
                    image_count = image_count.saturating_add(images.len());
                    for img in images {
                        image_bytes = image_bytes.saturating_add(img.bytes.len());
                    }
                }
                providers::AgentMessage::Assistant { text, tool_calls, .. } => {
                    msg_bytes = msg_bytes.saturating_add(text.len());
                    for tc in tool_calls {
                        msg_bytes = msg_bytes.saturating_add(tc.name.len()).saturating_add(bounded_json(&tc.arguments, MAX_PROMPT)?.len());
                    }
                }
                providers::AgentMessage::ToolResults { results } => {
                    for tr in results {
                        msg_bytes = msg_bytes.saturating_add(tr.name.len()).saturating_add(bounded_json(&tr.output, MAX_PROMPT)?.len());
                        image_count = image_count.saturating_add(tr.images.len());
                        for img in &tr.images {
                            image_bytes = image_bytes.saturating_add(img.bytes.len());
                        }
                    }
                }
            }
        }
        if image_count > 16 || image_bytes > 8 * 1024 * 1024 {
            return Err("AI multimodal turn images exceed the 16-image or 8-MiB limit".into());
        }
        let mut tools_bytes = 0usize;
        for t in &request.tools {
            tools_bytes = tools_bytes
                .saturating_add(t.name.len())
                .saturating_add(t.description.len())
                .saturating_add(bounded_json(&t.parameters, MAX_PROMPT)?.len());
        }
        let input = request.system.len().saturating_add(msg_bytes).saturating_add(tools_bytes);
        if input > MAX_PROMPT {
            return Err("AI prompt exceeds the bounded context limit".into());
        }
        self.ledger.clone().reserve_with_output(input, image_count, selection.provider, false, None, Some(request.max_output_tokens))?;
        let resources = crate::resources::LocalRequestGuard::acquire(selection, &request.resources)?;
        let key = selected_key(self.keys, selection);
        let reservation = self.ledger.reserve_with_output(input, image_count, selection.provider, false, None, Some(request.max_output_tokens))?;
        self.usage()?;
        let result = guarded(|| self.provider.agent_turn(selection, key.as_ref().map(providers::SecretString::expose), request, self.cancel));
        drop(resources);
        self.ledger.record(reservation, result.as_ref().ok().map(|response| &response.usage));
        self.usage()?;
        check_cancel(self.cancel)?;
        let response = result?;
        if let Some(error) = &response.error {
            return Err(error.clone());
        }
        if response.text.len() > MAX_JSON {
            return Err("AI response exceeds the 4-MiB limit".into());
        }
        Ok(response)
    }
    pub(crate) fn extend_local_comfy_request_allowance(&mut self, settings: &AiSettings, additional_images: usize) -> Result<(), String> {
        if settings.image_model.provider != ProviderKind::ComfyUi || settings.verify_generated_images || additional_images == 0 {
            return Ok(());
        }
        let add = u32::try_from(additional_images).unwrap_or(u32::MAX);
        let needed = self.ledger.summary().requests.saturating_add(add).clamp(1, 10_000);
        if needed > settings.budget.max_requests {
            let mut raised = settings.budget.clone();
            raised.max_requests = needed;
            self.ledger.update_budget(raised, settings.image_rates.as_ref())?;
        }
        Ok(())
    }
    pub(crate) fn can_admit_deferred_image_batches(
        &self,
        title: &str,
        batches: &[crate::doc_tools::DeferredImagePlan],
        settings: &AiSettings,
        style_profile: Option<&designcraft_ai_types::DesignProfile>,
    ) -> Result<(), String> {
        if batches.iter().all(|b| b.images.is_empty()) {
            return Ok(());
        }
        if settings.output_mode == OutputMode::NativeOnly {
            return Err("Native-only output forbids generated raster images".into());
        }
        validate_image_review_settings(settings)?;
        let style_context = illustration_style(style_profile)?;
        let mut projected = self.ledger.clone();
        let mut sim_cache = BTreeMap::new();
        for batch in batches {
            if batch.images.is_empty() {
                continue;
            }
            let mut plan = ProductionPlan { title: title.to_string(), commands: batch.companion_commands.clone(), images: batch.images.clone() };
            crate::image_repetition::normalize(&mut plan)?;
            image_command_budget(&plan, None)?;
            if let Some(root) = &self.cache_root {
                for placement in &plan.images {
                    let key = image_memory_key(placement, &style_context, settings)?;
                    if let std::collections::btree_map::Entry::Vacant(entry) = sim_cache.entry(key) {
                        let disk_key = self.image_disk_key(placement, &style_context, None, settings)?;
                        if let Ok(Some(image)) = crate::production_cache::load_image(root, &disk_key)
                            && validate_image(&image).is_ok()
                        {
                            entry.insert(image);
                        }
                    }
                }
            }
            for placement in &plan.images {
                if placement.instance_count() > 1 && declared_image_slot(placement, None).is_some() {
                    return Err("Counted single-subject frames cannot also be pinned atlas crops; no images were requested".into());
                }
                self.print_image_size(placement, None, &settings.image_model)?;
            }
            project_image_requests(&mut projected, &plan, settings, None, &style_context, &sim_cache)?;
            for placement in &plan.images {
                let key = image_memory_key(placement, &style_context, settings)?;
                sim_cache.entry(key).or_insert_with(|| providers::GeneratedImage {
                    bytes: Vec::new(),
                    filename: "sim.png".into(),
                    mime_type: "image/png".into(),
                });
            }
        }
        Ok(())
    }
    pub(crate) fn expand_deferred_image_batches(
        &mut self,
        title: &str,
        batches: &[crate::doc_tools::DeferredImagePlan],
        settings: &AiSettings,
        style_profile: Option<&designcraft_ai_types::DesignProfile>,
        scratch: &mut Session,
        min_ppi: f64,
    ) -> Result<Vec<Vec<EngineAction>>, String> {
        // Pre-flight ALL batches together before purchasing any image in batch 0.
        self.can_admit_deferred_image_batches(title, batches, settings, style_profile)?;
        let mut image_cache = BTreeMap::new();
        let mut image_bytes = 0usize;
        let mut chunks = Vec::with_capacity(batches.len());
        for (idx, batch) in batches.iter().enumerate() {
            check_cancel(self.cancel)?;
            let current_doc = scratch.doc().map_err(|e| e.to_string())?.clone();
            let raw_plan = ProductionPlan {
                title: if batches.len() == 1 { title.to_string() } else { format!("{title} ({}/{})", idx.saturating_add(1), batches.len()) },
                commands: batch.companion_commands.clone(),
                images: batch.images.clone(),
            };
            let expanded =
                self.expand_images_with_geometry(raw_plan, settings, &mut image_cache, &mut image_bytes, style_profile, None, Some(&current_doc))?;
            scratch
                .execute(
                    "production.apply",
                    &json!({
                        "commands": expanded.commands,
                        "outputMode": "native_with_images",
                        "label": expanded.title,
                        "requirePreflight": true,
                        "minPpi": min_ppi,
                    }),
                )
                .map_err(|e| e.to_string())?;
            chunks.push(expanded.commands);
        }
        Ok(chunks)
    }
    pub(super) fn generate(&mut self, selection: &ModelSelection, request: &GenerationRequest) -> Result<GenerationResponse, String> {
        let response = self.generate_plan_candidate(selection, request)?;
        if let Some(error) = &response.error {
            return Err(error.clone());
        }
        Ok(response)
    }
    pub(super) fn generate_plan_candidate(&mut self, selection: &ModelSelection, request: &GenerationRequest) -> Result<GenerationResponse, String> {
        check_cancel(self.cancel)?;
        let input = request.system.len().saturating_add(request.prompt.len()).saturating_add(bounded_json(&request.json_schema, MAX_PROMPT)?.len());
        if input > MAX_PROMPT {
            return Err("AI prompt exceeds the bounded context limit".into());
        }
        self.ledger.clone().reserve_with_output(input, request.images.len(), selection.provider, false, None, Some(request.max_output_tokens))?;
        let resources = crate::resources::LocalRequestGuard::acquire(selection, &request.resources)?;
        let key = selected_key(self.keys, selection);
        let reservation =
            self.ledger.reserve_with_output(input, request.images.len(), selection.provider, false, None, Some(request.max_output_tokens))?;
        self.usage()?;
        let result = guarded(|| self.provider.generate(selection, key.as_ref().map(providers::SecretString::expose), request, self.cancel));
        drop(resources);
        self.ledger.record(reservation, result.as_ref().ok().map(|response| &response.usage));
        self.usage()?;
        check_cancel(self.cancel)?;
        let response = result?;
        if response.text.len() > MAX_JSON {
            return Err("AI response exceeds the 4-MiB limit".into());
        }
        Ok(response)
    }
    pub(super) fn production(
        &mut self,
        request: TaskRequest,
        mut context: Value,
        catalog: Value,
        skills: Vec<SkillSnapshot>,
        base: DocState,
        prefs: Prefs,
    ) -> Result<Product, String> {
        self.progress("Preparing native production context", 0.05)?;
        let scope =
            context.get("batch_scope").map(|value| serde_json::from_value::<BatchScope>(value.clone()).map_err(|e| e.to_string())).transpose()?;
        validate_prompt(&request.prompt)?;
        if request.settings.output_mode == OutputMode::NativeWithImages {
            validate_image_review_settings(&request.settings)?;
        }
        self.cache_root = context.get("_production_cache_root").and_then(Value::as_str).map(PathBuf::from);
        self.print = crate::print_contract::PrintContract::from_prompt(&request.prompt)?;
        if let Some(message) = context.get("existing_document_print_notice").and_then(Value::as_str) {
            self.diagnostics(vec![json!({"kind":"existingDocumentPrintGeometry","severity":"warning","message":message})])?;
        }
        if let Some(contract) = &self.print {
            let fonts = contract.select_fonts(&base)?;
            context
                .as_object_mut()
                .ok_or("Invalid resolved print-font context")?
                .insert("resolved_print_fonts".into(), serde_json::to_value(&fonts).map_err(|error| error.to_string())?);
            self.diagnostics(fonts.notices.into_iter().map(|message| json!({"kind":"printFontFallback","message":message})).collect())?;
            self.diagnostics(
                contract.warnings().into_iter().map(|message| json!({"kind":"printRequirement","severity":"warning","message":message})).collect(),
            )?;
        }
        let explicit_script = crate::script_contract::parse(&request.prompt)?.is_some();
        let adaptive_layout = explicit_script
            || self.print.is_some()
            || unbound_math_request(&request.prompt, &skills)
            || continuation_style_geometry_differs(&request, &base, &skills);
        let current_document = matches!(request.target, designcraft_ai_types::protocol::TaskTarget::CurrentDocument);
        if current_document || matches!(request.target, designcraft_ai_types::protocol::TaskTarget::ContinueBook) {
            opened_document_geometry(&mut context, &base)?;
        }
        let profiles = skills.iter().filter_map(|s| s.design_profile.as_ref()).collect::<Vec<_>>();
        if profiles.len() > 1 {
            return Err("Conflicting design styles in task snapshot".into());
        }
        let profile = profiles.first().copied();
        if let Some(profile) = profile {
            profile.validate()?;
            if !current_document && !adaptive_layout {
                preflight_profile_image_ratios(profile, &request.settings)?;
            }
        }
        if current_document && scope.is_none() && !self.planning_only && !explicit_script && self.provider.supports_agent_loop() {
            return crate::agent_loop::run_open_document_agent_loop(self, &request, &context, &catalog, &skills, &base, &prefs);
        }
        if !adaptive_layout
            && let Some(style) = skills.iter().find(|skill| crate::template_plan::eligible(skill))
            && matches!(
                request.target,
                designcraft_ai_types::protocol::TaskTarget::NewA4 { .. } | designcraft_ai_types::protocol::TaskTarget::ContinueBook
            )
        {
            let review_group = context.get("batch_scope").is_some();
            let scope = match scope {
                Some(scope) => scope,
                None => BatchScope { start: 0, count: u32::try_from(base.doc.spreads.len()).map_err(|_| "Invalid native page count")? },
            };
            // Content mode still consumes the selected skills' actual guidance and
            // references, without sending their command templates or the engine catalog.
            let guidance = compact_skill_guidance(&skills)?;
            let total_pages = match request.target {
                designcraft_ai_types::protocol::TaskTarget::NewA4 { pages } => pages,
                _ => u32::try_from(base.doc.spreads.len()).unwrap_or(scope.start.saturating_add(scope.count)),
            }
            .max(scope.start.saturating_add(scope.count));
            let mut template_context = context;
            let object = template_context.as_object_mut().ok_or("Invalid template production context")?;
            object.insert("selected_skill_guidance".into(), json!(guidance));
            object.insert(
                "skill_tool_mastermind".into(),
                crate::template_plan::skill_tool_mastermind_brief(
                    &request.prompt,
                    &skills,
                    scope,
                    total_pages,
                    Some(crate::runtime::book_binding(&request.settings, &request.prompt)),
                ),
            );
            if scope.count > crate::template_plan::capacity(style, request.settings.output_mode)? {
                if !review_group {
                    return Err("Template pages exceed one internal native plan; use a human-reviewed book batch".into());
                }
                return self.template_review_group(request, template_context, style, base, prefs, scope, total_pages);
            }
            object.insert("curriculum_blueprint".into(), crate::template_plan::curriculum_blueprint(&request.prompt, style, scope, total_pages)?);
            return self.template_production(request, template_context, style, base, prefs, scope);
        }
        if let Some(scope) = scope
            && scope.count > if adaptive_layout { 1 } else { generic_page_capacity(request.settings.output_mode) }
        {
            return self.generic_review_group(&request, &context, &catalog, &skills, &base, &prefs, scope);
        }
        let plan = self.generic_production_plan(&request, &context, &catalog, &skills, &base, &prefs, scope)?;
        if let Some(scope) = scope {
            self.progress("Preparing complete review proofs", 0.9)?;
            let proofs = batch_proofs_with_layout(&plan, &base, &prefs, &request.settings, &request.prompt, self.cancel, scope)?;
            Ok(Product::BatchPlan { plan, proofs })
        } else {
            Ok(Product::Plan(plan))
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn generic_production_plan(
        &mut self,
        request: &TaskRequest,
        context: &Value,
        catalog: &Value,
        skills: &[SkillSnapshot],
        base: &DocState,
        prefs: &Prefs,
        scope: Option<BatchScope>,
    ) -> Result<ProductionPlan, String> {
        self.generic_production_plan_with_min_requests(request, context, catalog, skills, base, prefs, scope, 1, false)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn generic_production_plan_with_min_requests(
        &mut self,
        request: &TaskRequest,
        context: &Value,
        catalog: &Value,
        skills: &[SkillSnapshot],
        base: &DocState,
        prefs: &Prefs,
        scope: Option<BatchScope>,
        min_text_requests: u32,
        defer_images: bool,
    ) -> Result<ProductionPlan, String> {
        let current_document = matches!(request.target, designcraft_ai_types::protocol::TaskTarget::CurrentDocument);
        let opened_document = current_document || matches!(request.target, designcraft_ai_types::protocol::TaskTarget::ContinueBook);
        let profile = skills.iter().find_map(|skill| skill.design_profile.as_ref());
        let explicit_script = crate::script_contract::parse(&request.prompt)?.is_some();
        let adaptive_layout = explicit_script
            || self.print.is_some()
            || unbound_math_request(&request.prompt, skills)
            || continuation_style_geometry_differs(request, base, skills);
        let geometry_profile = if opened_document || adaptive_layout { None } else { profile };
        let allow_geometry_edits = current_document && scope.is_none() && prompt_requests_document_geometry_edit(&request.prompt);
        let geometry_check = if opened_document || adaptive_layout {
            GeometryCheck::OpenedDocument { allow_geometry_edits }
        } else {
            profile.map_or(GeometryCheck::Engine, GeometryCheck::Profile)
        };
        // Adaptive commands are planned against the actual document, not compiled
        // from these fixed templates. Keep the host snapshots intact and omit only
        // their unused executable examples from this model-context copy.
        let mut planner_skills = skills.to_vec();
        if adaptive_layout {
            for skill in &mut planner_skills {
                skill.templates.clear();
            }
        }
        let mut system =
            production_system(catalog, &planner_skills, request.settings.output_mode, opened_document || adaptive_layout, adaptive_layout)?;
        if adaptive_layout {
            system.push_str("\nADAPTIVE SCRIPT/PRINT LAYOUT: actual document geometry and any explicit page script override profile page_mm, margin_mm and fixed example compositions. Keep the pinned profile's visual identity and complete guidance while constructing the required native page content. Executable template examples remain local host inputs and are not required for this adaptive command planner.");
        }
        if let Some(scope) = scope {
            system.push_str(&format!("\nBATCH SCOPE IS MANDATORY: create only spreads {}..{} (exclusive end); every object needs explicit spread; every object/story reference must name an earlier result created in this batch. No page/global/style/swatches operations. Render every requested page natively before the human proof checkpoint.",scope.start,scope.start.saturating_add(scope.count)));
        }
        if context.get("review_scope").is_some() {
            system.push_str("\nINTERNAL REVIEW PART: preserve the entire user's review and brief while producing only document.internal_scope. The document.remainingImageSources field is the upper bound for independent image sources in this part after reserving its text request; token, cost and resource admission still apply. All native words and layout remain editable. Internal parts do not approve or publish pages.");
        }
        if crate::script_contract::parse(&request.prompt)?.is_some() {
            system.push_str("\nAUTHORITATIVE NUMBERED PAGE SCRIPT: implement only the scoped PAGE blocks exactly. Their page order, printable English and Arabic copy (including diacritics and quantities) override suggested skill outlines. Preserve designated print copy in final frame.create text or story.setText. Put Arabic and English in separate native frames. Reserve local fitting space; keep at most 400 authored commands. Use optional activities for every five/ten/twenty-frame: {page,rect,frame:five|ten|twenty,quantity,filled}; the host creates exact native cells/counters. Never draw counting grids with arbitrary static template parts or bake numerals/instructions into generated images. Illustrated quantities are assembled with repeat_groups/repeat_rects from ONE isolated subject per generated source. Do not ask diffusion for multiple counted subjects. Native-only plans use editable vector subjects. Leave task labels, headers, footers and answer fields editable. Do not claim pixel-level semantic checks.");
        }
        if current_document {
            system.push_str("\nEXISTING DOCUMENT EDITING CONTRACT: document.objects lists the opened document's existing native items with their numeric id, spread, page, bounds, content (text|graphic|group|unassigned), shape, story (numeric StoryId for text frames), text preview, fillSwatch, fillTint, strokeSwatch, strokeWeight, locked flag, parentGroup, typography, and tables (numeric table ID, rows, cols, cells). When the user asks to modify, restyle, translate, replace, align, transform, or delete existing elements in the opened document, target those existing items directly by their numeric id (e.g. object.fill, object.stroke, object.gradient, object.cornerOptions, object.opacity, object.dropShadow, object.feather, object.gradientFeather, object.effects, object.textWrap, object.arrange, object.align, object.distribute, object.pathfinder, object.makeCompoundPath, object.releaseCompoundPath, object.fit, object.ungroup, edit.stepAndRepeat, transform.set, transform.move, transform.rotate, transform.scale, transform.flip, style.object.apply, edit.clear), target existing text stories by their numeric story ID (story.setText, story.setDirection, text.select + type.char/type.para/style.paragraph.apply/style.character.apply, list.define, text.thread/text.unthread, or find.change), and target existing tables by their numeric story + table IDs (table.setCell, table.insertRow, table.insertColumn, table.deleteRow, table.deleteColumn, table.merge, table.unmerge, table.setRowHeight, table.setColumnWidth, table.distributeColumns, table.distributeRows, table.options, style.table.apply, style.cell.apply). If the user explicitly asks to add pages or adjust margins/page setup, use layout.pages.insert, layout.marginsAndColumns, or layout.documentSetup. To replace or populate an existing graphic frame with a generated illustration in NativeWithImages mode, emit an images[] entry whose page (0-based spread index) and rect match the existing graphic frame's spread and bounds; the host places the generated image directly into that frame. Only create new frames when adding new design elements or replacing deleted ones; never stack duplicate text or graphic frames on top of unchanged existing frames.");
        }
        let scoped_prompt =
            scope.map(|scope| crate::script_contract::scoped_prompt(&request.prompt, scope)).transpose()?.unwrap_or_else(|| request.prompt.clone());
        let mut document_context = context.clone();
        if opened_document || adaptive_layout {
            opened_document_geometry(&mut document_context, base)?;
        }
        if adaptive_layout
            && serde_json::to_vec(&document_context).is_ok_and(|bytes| bytes.len() < MAX_PROMPT / 3)
            && let Some(object) = document_context.as_object_mut()
            && !object.contains_key("skill_tool_mastermind")
        {
            let eff_scope = scope.unwrap_or(BatchScope { start: 0, count: u32::try_from(base.doc.spreads.len()).unwrap_or(1).max(1) });
            let total_pages = u32::try_from(base.doc.spreads.len()).unwrap_or(eff_scope.start.saturating_add(eff_scope.count)).max(1);
            object.insert(
                "skill_tool_mastermind".into(),
                crate::template_plan::skill_tool_mastermind_brief(
                    &request.prompt,
                    skills,
                    eff_scope,
                    total_pages,
                    Some(crate::runtime::book_binding(&request.settings, &request.prompt)),
                ),
            );
        }
        if explicit_script && let Some(scope) = scope {
            document_context
                .as_object_mut()
                .ok_or("Invalid explicit-copy context")?
                .insert("source_copy".into(), crate::script_contract::source_copy_context(&request.prompt, scope)?);
            system.push_str("\nNATIVE SOURCE COPY BINDINGS: document.source_copy contains immutable print-copy segments and indexed words. Prefer copy_bindings:[{frame:ZERO_BASED_COMMAND_INDEX,copy_id,word_range:[START,EXCLUSIVE_END]}] with an empty text placeholder in each referenced frame.create. The host inserts the exact source words, punctuation and Arabic marks locally. Split a segment across its designed native frames using complete non-overlapping word ranges; this supports bilingual interleaving and separate number-chart cards without retyping or changing copy. Each bound frame must be on the copy segment's page. Bind all words of any segment you bind, exactly once. Do not overwrite bound stories or their reserved provenance labels. Font/direction/alignment commands still apply normally. Unbound copy is checked literally as before.");
        }
        if request.settings.facing_pages
            && let Some(scope) = scope
        {
            document_context
                .as_object_mut()
                .ok_or("Invalid facing production context")?
                .insert("facing_page_context".into(), crate::script_contract::facing_context(&request.prompt, scope)?);
            system.push_str("\nFACING-PAGE COORDINATION: document.facing_page_context provides absolute companion pages for occasional magazine-style relationships when authored content benefits from a linked pair. Keep each lesson complete. Companion members marked context_only are reference data only: do not generate, replace, approve or advance them, and never widen the mandatory batch scope. Linking every facing pair is not required.");
        }
        system.push_str("\nNATIVE ACTION HEADROOM: the full expanded page must fit 512 native actions. Each five/ten/twenty-frame activity is compiled locally using two actions per cell/token plus up to three shared formatting actions. Each repeated single-subject set uses four initial actions and two per equal-sized copy (four per resized copy). Leave room for all images and measured text recovery; preserve every source word, quantity and the user's review-page count.");
        if request.settings.output_mode == OutputMode::NativeWithImages {
            system.push('\n');
            system.push_str(crate::image_repetition::POLICY);
        }
        let original = bounded_json(&json!({"request":scoped_prompt,"target":request.target,"document":document_context}), MAX_PROMPT)?;
        let mut schema = plan_schema(catalog, request.settings.output_mode)?;
        if scope.is_some() {
            schema["properties"]["activities"] = json!({"type":"array","maxItems":16,"items":crate::script_contract::activity_schema()});
            if explicit_script {
                schema["properties"]["copy_bindings"] = json!({"type":"array","maxItems":256,"items":crate::script_contract::copy_binding_schema()});
            }
        }
        let mut generation = GenerationRequest {
            system,
            prompt: original.clone(),
            json_schema: schema,
            max_output_tokens: request.settings.budget.max_output_tokens,
            images: Vec::new(),
            resources: request.settings.resources.clone(),
        };
        let cache_key = self.plan_key(request, &generation, base)?;
        let required_min_ppi = self.print.as_ref().and_then(|contract| contract.min_image_ppi());
        if let Some(plan) = self.cached_plan(cache_key.as_deref(), request, context, base, prefs, scope, catalog, geometry_profile)? {
            if let Some(profile) = geometry_profile {
                crate::design::check_image_slots(base, &plan.images, profile)?;
            }
            let text_scope = if plan.images.is_empty() { scope } else { None };
            simulate_styled_with_ppi(&plan, base, prefs, request.settings.output_mode, self.cancel, text_scope, geometry_check, required_min_ppi)?;
            if self.planning_only || defer_images {
                return Ok(plan);
            }
            let expanded = self.expand_images_with_geometry(
                plan,
                &request.settings,
                &mut BTreeMap::new(),
                &mut 0,
                profile,
                geometry_profile,
                opened_document.then_some(base),
            )?;
            simulate_styled_with_ppi(&expanded, base, prefs, request.settings.output_mode, self.cancel, scope, geometry_check, required_min_ppi)?;
            self.audit_print(&expanded, base, prefs)?;
            return Ok(expanded);
        }
        if !self.planning_only {
            validate_monetary_image_rates(&request.settings)?;
        }
        if self.ledger.remaining_requests() < min_text_requests {
            return Err("AI request limit reached".into());
        }
        let mut image_cache = BTreeMap::new();
        let mut image_bytes = 0usize;
        let repair_attempts = if request.settings.allow_paid_repairs { request.settings.budget.repair_attempts } else { 0 };
        for attempt in 0..=repair_attempts {
            let stage = match (attempt, scope) {
                (0, Some(s)) if s.count == 1 => format!("Generating editable native layout for page {}", u64::from(s.start) + 1),
                (0, Some(s)) => {
                    format!("Generating editable native layout for pages {}–{}", u64::from(s.start) + 1, u64::from(s.start) + u64::from(s.count))
                }
                (0, None) => "Generating editable native layout".into(),
                (n, Some(s)) if s.count == 1 => {
                    format!("Repairing native layout validation for page {} (attempt {n} of {repair_attempts})", u64::from(s.start) + 1)
                }
                (n, Some(s)) => format!(
                    "Repairing native layout validation for pages {}–{} (attempt {n} of {repair_attempts})",
                    u64::from(s.start) + 1,
                    u64::from(s.start) + u64::from(s.count)
                ),
                (n, None) => format!("Repairing native layout validation (attempt {n} of {repair_attempts})"),
            };
            self.progress(&stage, 0.15)?;
            let response = self.generate_plan_candidate(&request.settings.text_model, &generation)?;
            let has_provider_error = response.error.is_some();
            let parsed = if let Some(error) = response.error.clone() {
                Err(error)
            } else {
                let native_text = scope
                    .map(|scope| {
                        parse_json::<Value>(&response.text)
                            .and_then(|value| bounded_json(&value, MAX_JSON))
                            .and_then(|text| {
                                if explicit_script {
                                    crate::script_contract::compile_native_plan_for_brief(&text, scope, &request.prompt)
                                } else {
                                    crate::script_contract::compile_native_plan(&text, scope)
                                }
                            })
                            .and_then(|plan| bounded_json(&serde_json::to_value(plan).map_err(|e| e.to_string())?, MAX_JSON))
                    })
                    .transpose();
                native_text.and_then(|text| parse_plan(text.as_deref().unwrap_or(&response.text), request.settings.output_mode, catalog)).and_then(
                    |mut plan| {
                        prepare_editorial_plan_with_mode(&mut plan, &request.prompt, context, None, request.settings.arabic_tashkeel)?;
                        if let Some(contract) = &self.print {
                            self.diagnostics(crate::native_print_fonts::apply(&mut plan, base, prefs, contract)?)?;
                        } else if let Some(explicit_contract) =
                            explicit_arabic_font_contract(base, request.settings.arabic_font.as_deref(), &request.prompt)
                        {
                            self.diagnostics(crate::native_print_fonts::apply(&mut plan, base, prefs, &explicit_contract)?)?;
                        }
                        if let Some(scope) = scope {
                            crate::script_contract::validate_plan(&request.prompt, scope, &plan)?;
                            crate::script_contract::anchor_images(&request.prompt, scope, &mut plan)?;
                        }
                        self.diagnostics(crate::native_layout::prepare(&mut plan, base, prefs, self.cancel)?)?;
                        validate_previous_body_layout(&plan, base, prefs, scope, &request.prompt)?;
                        image_command_budget(&plan, geometry_profile)?;
                        if let Some(profile) = geometry_profile {
                            crate::design::check_image_slots(base, &plan.images, profile)?;
                        }
                        if let Some(scope) = scope {
                            validate_batch_plan(&plan, scope)?;
                        }
                        Ok(plan)
                    },
                )
            };
            let (previous, validation) = match parsed {
                Ok(raw_plan) => {
                    // Repairs consume the authored envelope, including copy_bindings and
                    // activities. Host labels, fonts and recoveries are compiled afresh.
                    let previous = response.text.clone();
                    // Validate native typography before spending on images. Page completeness
                    // is checked on the final illustrated draft, since a page can be image-only.
                    let text_scope = if raw_plan.images.is_empty() { scope } else { None };
                    let text_check = simulate_styled_with_ppi(
                        &raw_plan,
                        base,
                        prefs,
                        request.settings.output_mode,
                        self.cancel,
                        text_scope,
                        geometry_check,
                        required_min_ppi,
                    );
                    if let Err(error) = text_check {
                        (previous, error)
                    } else {
                        if self.planning_only || defer_images {
                            self.cache_plan(cache_key.as_deref(), &raw_plan)?;
                            return Ok(raw_plan);
                        }
                        // Image failures fail the whole task. Never publish a partially illustrated layout.
                        let expanded = self.expand_images_with_geometry(
                            raw_plan.clone(),
                            &request.settings,
                            &mut image_cache,
                            &mut image_bytes,
                            profile,
                            geometry_profile,
                            opened_document.then_some(base),
                        )?;
                        self.progress("Validating commands and print preflight on a private document", 0.85)?;
                        match simulate_styled_with_ppi(
                            &expanded,
                            base,
                            prefs,
                            request.settings.output_mode,
                            self.cancel,
                            scope,
                            geometry_check,
                            required_min_ppi,
                        )
                        .and_then(|()| self.audit_print(&expanded, base, prefs))
                        {
                            Ok(()) => {
                                self.cache_plan(cache_key.as_deref(), &raw_plan)?;
                                check_cancel(self.cancel)?;
                                return Ok(expanded);
                            }
                            Err(error) => (previous, error),
                        }
                    }
                }
                Err(error) => (response.text, error),
            };
            check_cancel(self.cancel)?;
            // Keep bounded local evidence, never a validated cache entry or
            // executable input. Large plans expose only bounded native text.
            let rejected_text =
                parse_json::<Value>(&previous).ok().and_then(|value| {
                    let text: Vec<_> = value.get("commands")?.as_array()?.iter().enumerate().filter_map(|(index, action)| {
                    let command = action.get("command")?.as_str()?;
                    if !matches!(command, "frame.create" | "story.setText") { return None; }
                    let params = action.get("params")?;
                    let text = params.get("text")?.as_str()?;
                    Some(json!({"command_index":index,"command":command,"spread":params.get("spread"),"story":params.get("story"),"text":text}))
                }).collect();
                    bounded_json(&json!(text), 8192).ok()
                });
            let rejected_response_path = if let Some(root) = &self.cache_root
                && !previous.is_empty()
            {
                match crate::production_cache::store_rejected_response(root, &previous) {
                    Ok(path) => path,
                    Err(error) => {
                        self.cache_notice(format!("Could not retain private rejected-response evidence: {error}"))?;
                        None
                    }
                }
            } else {
                None
            };
            self.diagnostics(vec![json!({
                "kind":"nativePlanningValidation", "attempt":attempt.saturating_add(1),
                "message":validation, "rejected_plan_included":previous.len() <= 8192,
                "rejected_plan":if previous.len() <= 8192 {Some(previous.as_str())} else {None},
                "rejected_native_text":if previous.len() > 8192 {rejected_text} else {None},
                "rejected_response_path":rejected_response_path,
                "scope":scope,
            })])?;
            if attempt == repair_attempts {
                return Err(if has_provider_error { validation } else { format!("AI plan rejected: {validation}") });
            }
            generation.prompt = repair_prompt(&original, &previous, &validation)?;
        }
        Err("AI plan was not produced".into())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn generic_review_group(
        &mut self,
        request: &TaskRequest,
        context: &Value,
        catalog: &Value,
        skills: &[SkillSnapshot],
        base: &DocState,
        prefs: &Prefs,
        review: BatchScope,
    ) -> Result<Product, String> {
        let end = review_end(review)?;
        validate_batch_result(base, base, review)?;
        let profile = skills.iter().find_map(|skill| skill.design_profile.as_ref());
        let opened = matches!(
            request.target,
            designcraft_ai_types::protocol::TaskTarget::CurrentDocument | designcraft_ai_types::protocol::TaskTarget::ContinueBook
        );
        let adaptive = crate::script_contract::parse(&request.prompt)?.is_some()
            || self.print.is_some()
            || unbound_math_request(&request.prompt, skills)
            || continuation_style_geometry_differs(request, base, skills);
        let geometry_profile = if opened || adaptive { None } else { profile };
        let geometry_check = if opened || adaptive {
            GeometryCheck::OpenedDocument { allow_geometry_edits: false }
        } else {
            profile.map_or(GeometryCheck::Engine, GeometryCheck::Profile)
        };
        let capacity =
            if crate::script_contract::parse(&request.prompt)?.is_some() || self.print.is_some() || unbound_math_request(&request.prompt, skills) {
                1
            } else {
                generic_page_capacity(request.settings.output_mode)
            };
        let mut scratch = Session::new();
        scratch.prefs = prefs.clone();
        scratch.add_document(base.clone());
        let mut plans = Vec::new();
        let mut plan_bytes = 0usize;
        let mut next = review.start;
        while next < end {
            check_cancel(self.cancel)?;
            let scope = BatchScope { start: next, count: capacity.min(end - next) };
            let mut chunk_context = internal_template_context(context, scratch.doc().map_err(|e| e.to_string())?, review, scope)?;
            let remaining_requests = request.settings.budget.max_requests.saturating_sub(self.ledger.summary().requests);
            let source_limit = if request.settings.output_mode == OutputMode::NativeOnly {
                0
            } else {
                let per_source = if request.settings.verify_generated_images { 2 } else { 1 };
                MAX_IMAGES.min(
                    usize::try_from(remaining_requests.saturating_sub(1) / per_source).map_err(|_| "Invalid remaining image request allowance")?,
                )
            };
            let object = chunk_context.as_object_mut().ok_or("Invalid internal generic review context")?;
            object.insert("batch_scope".into(), json!(scope));
            object.insert("remainingImageSources".into(), json!(source_limit));
            object.insert("remainingTaskRequests".into(), json!(remaining_requests));
            // Explicit scope validation is independent of the context marker and proof policy.
            // Every part uses this Runner's same ledger, keys, cancellation and pinned skills.
            let remaining_chunks = (end - next).div_ceil(capacity.max(1));
            let plan = self.generic_production_plan_with_min_requests(
                request,
                &chunk_context,
                catalog,
                skills,
                scratch.doc().map_err(|e| e.to_string())?,
                prefs,
                Some(scope),
                remaining_chunks,
                true,
            )?;
            let bytes = serde_json::to_vec(&plan).map_err(|e| e.to_string())?.len();
            plan_bytes =
                plan_bytes.checked_add(bytes).filter(|total| *total <= MAX_GROUP_PLAN_BYTES).ok_or("Review plans exceed the 256-MiB limit")?;
            if plans.len() >= MAX_REVIEW_PAGES as usize {
                return Err("Review plan count exceeds its bounded page limit".into());
            }
            let min_ppi = self.print.as_ref().and_then(|contract| contract.min_image_ppi()).unwrap_or(150.0);
            scratch.execute("production.apply", &json!({"commands":plan.commands,"outputMode":request.settings.output_mode,"label":plan.title,"requirePreflight":true,"minPpi":min_ppi}))
                .map_err(|e| e.to_string())?;
            plans.push((plan, scope));
            next = next.checked_add(scope.count).ok_or("Internal generic review range overflow")?;
        }
        validate_batch_result(base, scratch.doc().map_err(|e| e.to_string())?, review)?;
        let style_context = illustration_style(profile)?;
        let mut projected = self.ledger.clone();
        let mut cached = BTreeMap::new();
        let mut cached_bytes = 0usize;
        for (plan, _) in &plans {
            for image in &plan.images {
                self.print_image_size(image, geometry_profile, &request.settings.image_model)?;
            }
            self.preload_images(plan, &request.settings, geometry_profile, &style_context, &mut cached, &mut cached_bytes)?;
            project_image_requests(&mut projected, plan, &request.settings, geometry_profile, &style_context, &cached)?;
        }
        drop(scratch);
        let mut scratch = Session::new();
        scratch.prefs = prefs.clone();
        scratch.add_document(base.clone());
        let mut expanded_plans = Vec::with_capacity(plans.len());
        plan_bytes = 0;
        let required_min_ppi = self.print.as_ref().and_then(|contract| contract.min_image_ppi());
        let min_ppi = required_min_ppi.unwrap_or(150.0);
        for (plan, scope) in plans {
            check_cancel(self.cancel)?;
            let plan = self.expand_images_with_geometry(plan, &request.settings, &mut cached, &mut cached_bytes, profile, geometry_profile, None)?;
            let expanded_bytes = serde_json::to_vec(&plan).map_err(|error| error.to_string())?.len();
            plan_bytes = plan_bytes
                .checked_add(expanded_bytes)
                .filter(|bytes| *bytes <= MAX_GROUP_PLAN_BYTES)
                .ok_or("Expanded review plans exceed the 256-MiB limit")?;
            self.progress(&format!("Validating commands and print preflight for page {}", u64::from(scope.start) + 1), 0.8)?;
            simulate_styled_with_ppi(
                &plan,
                scratch.doc().map_err(|e| e.to_string())?,
                prefs,
                request.settings.output_mode,
                self.cancel,
                Some(scope),
                geometry_check,
                required_min_ppi,
            )?;
            self.audit_print(&plan, scratch.doc().map_err(|e| e.to_string())?, prefs)?;
            scratch
                .execute(
                    "production.apply",
                    &json!({"commands":plan.commands,"outputMode":request.settings.output_mode,"requirePreflight":true,"minPpi":min_ppi}),
                )
                .map_err(|e| e.to_string())?;
            expanded_plans.push(plan);
        }
        self.progress("Preparing complete review proofs from all internal page parts", 0.9)?;
        let proofs = review_proofs(&mut scratch, self.cancel, review, &request.settings, &request.prompt)?;
        Ok(Product::BatchGroup { plans: expanded_plans, proofs })
    }
}

impl Runner<'_> {
    pub(super) fn template_production(
        &mut self,
        request: TaskRequest,
        context: Value,
        style: &SkillSnapshot,
        base: DocState,
        prefs: Prefs,
        scope: BatchScope,
    ) -> Result<Product, String> {
        let required_min_ppi = self.print.as_ref().and_then(|contract| contract.min_image_ppi());
        let raw = self.template_content(&request, &context, style, &base, &prefs, scope)?;
        let profile = style.design_profile.as_ref().ok_or("Missing pinned design profile")?;
        let expanded = self.expand_images(raw, &request.settings, &mut BTreeMap::new(), &mut 0, Some(profile))?;
        self.audit_print(&expanded, &base, &prefs)?;
        self.progress("Checking editable native pages and preparing proofs", 0.85)?;
        simulate_styled_with_ppi(
            &expanded,
            &base,
            &prefs,
            request.settings.output_mode,
            self.cancel,
            Some(scope),
            GeometryCheck::Profile(profile),
            required_min_ppi,
        )?;
        check_cancel(self.cancel)?;
        if context.get("batch_scope").is_some() {
            let proofs = batch_proofs_with_layout(&expanded, &base, &prefs, &request.settings, &request.prompt, self.cancel, scope)?;
            Ok(Product::BatchPlan { plan: expanded, proofs })
        } else {
            Ok(Product::Plan(expanded))
        }
    }

    /// Plan and validate copy without purchasing any illustrations. Review groups
    /// finish this phase for every internal part before entering the image phase.
    pub(super) fn template_content(
        &mut self,
        request: &TaskRequest,
        context: &Value,
        style: &SkillSnapshot,
        base: &DocState,
        prefs: &Prefs,
        scope: BatchScope,
    ) -> Result<ProductionPlan, String> {
        self.template_content_with_min_requests(request, context, style, base, prefs, scope, 1)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn template_content_with_min_requests(
        &mut self,
        request: &TaskRequest,
        context: &Value,
        style: &SkillSnapshot,
        base: &DocState,
        prefs: &Prefs,
        scope: BatchScope,
        min_text_requests: u32,
    ) -> Result<ProductionPlan, String> {
        self.progress(
            &format!(
                "Planning content for pages {}–{} before image generation",
                u64::from(scope.start) + 1,
                u64::from(scope.start) + u64::from(scope.count)
            ),
            0.15,
        )?;
        crate::template_plan::preflight(style, scope, request.settings.output_mode)?;
        let profile = style.design_profile.as_ref().ok_or("Missing pinned design profile")?;
        preflight_profile_image_ratios(profile, &request.settings)?;
        let schema = crate::template_plan::schema_for_brief(style, scope, request.settings.output_mode, &request.prompt)?;
        let families: BTreeSet<String> = schema["properties"]["pages"]["items"]["anyOf"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|family| family["properties"]["family"]["enum"].as_array()?.first()?.as_str().map(str::to_owned))
            .collect();
        let previous = previous_template_family(style, base, scope, &families)?;
        let mut context = context.clone();
        context.as_object_mut().ok_or("Invalid template planning context")?.insert("previousTemplateFamily".into(), json!(previous));
        let prompt = bounded_json(&json!({"request":request.prompt,"scope":scope,"document":context}), MAX_PROMPT)?;
        let text_capacity = style
            .design_profile
            .as_ref()
            .into_iter()
            .flat_map(|profile| &profile.page_families)
            .map(|family| family.text_slots.iter().map(|slot| slot.max_chars).sum::<usize>())
            .max()
            .unwrap_or(0);
        let max_image_slots = style
            .design_profile
            .as_ref()
            .into_iter()
            .flat_map(|profile| &profile.page_families)
            .map(|family| family.image_slots.len())
            .max()
            .unwrap_or(0);
        let image_prompt_allowance = if request.settings.output_mode == OutputMode::NativeWithImages {
            u32::try_from(max_image_slots).unwrap_or(8).saturating_mul(512)
        } else {
            0
        };
        let per_page = u32::try_from(text_capacity)
            .map_err(|_| "Template content capacity overflow")?
            .saturating_mul(2)
            .saturating_add(128)
            .saturating_add(image_prompt_allowance)
            .max(1024);
        // Gemini, OpenAI o-series/GPT-5, and Ollama reasoning models count internal
        // thinking against the same output allowance. Preserve a bounded reasoning margin.
        let thinking_margin = if model_needs_thinking_margin(&request.settings.text_model) { 4096 } else { 0 };
        let output_limit =
            scope.count.saturating_mul(per_page).saturating_add(512).saturating_add(thinking_margin).min(request.settings.budget.max_output_tokens);
        let generation = GenerationRequest {
            system: format!(
                "{}\n{EDITORIAL_POLICY}\nWhen multiple declared families are available, avoid document.previousTemplateFamily on the first requested page and vary subsequent families, unless the user explicitly requests identical layouts. That field is inferred from actual native item labels; null means unknown, not an approval.",
                crate::template_plan::system_for_brief_with_options(
                    style,
                    &request.prompt,
                    request.settings.output_mode,
                    request.settings.arabic_tashkeel
                )?
            ),
            prompt,
            json_schema: schema,
            max_output_tokens: output_limit,
            images: vec![],
            resources: request.settings.resources.clone(),
        };
        let required_min_ppi = self.print.as_ref().and_then(|contract| contract.min_image_ppi());
        let cache_key = self.plan_key(request, &generation, base)?;
        if let Some(plan) = self.cached_plan(
            cache_key.as_deref(),
            request,
            &context,
            base,
            prefs,
            Some(scope),
            &designcraft_engine::cmd::production::catalog(),
            Some(profile),
        )? {
            let cached_valid = (|| {
                validate_template_family_sequence(&plan, style, previous.clone(), scope, &families, &request.prompt)?;
                crate::design::check_image_slots(base, &plan.images, profile)?;
                simulate_styled_with_ppi(
                    &plan,
                    base,
                    prefs,
                    request.settings.output_mode,
                    self.cancel,
                    Some(scope),
                    GeometryCheck::Profile(profile),
                    required_min_ppi,
                )
            })();
            match cached_valid {
                Ok(()) => return Ok(plan),
                Err(error) => {
                    check_cancel(self.cancel)?;
                    self.cache_notice(format!("Ignored stale cached template plan: {error}"))?;
                }
            }
        }
        if !self.planning_only && families.iter().any(|id| profile.page_families.iter().any(|f| &f.id == id && !f.image_slots.is_empty())) {
            validate_monetary_image_rates(&request.settings)?;
        }
        if self.ledger.remaining_requests() < min_text_requests {
            return Err("AI request limit reached".into());
        }
        let original_prompt = generation.prompt.clone();
        let mut generation = generation;
        let repairs = if request.settings.allow_paid_repairs { request.settings.budget.repair_attempts.min(4) } else { 0 };
        for attempt in 0..=repairs {
            let response = self.generate(&request.settings.text_model, &generation)?;
            self.progress("Compiling page content into native engine commands locally", 0.45)?;
            let mut step_notes = Vec::new();
            let total_pages_hint = match request.target {
                designcraft_ai_types::protocol::TaskTarget::NewA4 { pages } => pages,
                _ => u32::try_from(base.doc.spreads.len()).unwrap_or(scope.start.saturating_add(scope.count)),
            }
            .max(scope.start.saturating_add(scope.count));
            let validation: Result<ProductionPlan, String> = (|| {
                let mut raw = crate::template_plan::compile_for_brief_with_context(
                    &response.text,
                    style,
                    scope,
                    request.settings.output_mode,
                    &request.prompt,
                    crate::template_plan::TemplateCompileContext {
                        base: Some(base),
                        facing_pages: request.settings.facing_pages || base.doc.settings.facing_pages,
                        total_pages: total_pages_hint,
                        subject: None,
                    },
                )?;
                validate_template_family_sequence(&raw, style, previous.clone(), scope, &families, &request.prompt)?;
                prepare_editorial_plan_with_mode(&mut raw, &request.prompt, &context, None, request.settings.arabic_tashkeel)?;
                if let Some(contract) = &self.print {
                    step_notes.extend(crate::native_print_fonts::apply(&mut raw, base, prefs, contract)?);
                } else if let Some(explicit_contract) = explicit_arabic_font_contract(base, request.settings.arabic_font.as_deref(), &request.prompt)
                {
                    step_notes.extend(crate::native_print_fonts::apply(&mut raw, base, prefs, &explicit_contract)?);
                } else if let Some(auto_contract) = auto_preferred_arabic_font_contract(base, &raw) {
                    let mut candidate = raw.clone();
                    let candidate_ok = (|| -> Result<Vec<Value>, String> {
                        let mut notes = crate::native_print_fonts::apply(&mut candidate, base, prefs, &auto_contract)?;
                        notes.extend(crate::native_layout::prepare(&mut candidate, base, prefs, self.cancel)?);
                        validate_previous_body_layout(&candidate, base, prefs, Some(scope), &request.prompt)?;
                        crate::design::check_image_slots(base, &candidate.images, profile)?;
                        simulate_styled_with_ppi(
                            &candidate,
                            base,
                            prefs,
                            request.settings.output_mode,
                            self.cancel,
                            Some(scope),
                            GeometryCheck::Profile(profile),
                            required_min_ppi,
                        )?;
                        Ok(notes)
                    })();
                    if let Ok(notes) = candidate_ok {
                        step_notes.extend(notes);
                        return Ok(candidate);
                    }
                }
                step_notes.extend(crate::native_layout::prepare(&mut raw, base, prefs, self.cancel)?);
                validate_previous_body_layout(&raw, base, prefs, Some(scope), &request.prompt)?;
                crate::design::check_image_slots(base, &raw.images, profile)?;
                simulate_styled_with_ppi(
                    &raw,
                    base,
                    prefs,
                    request.settings.output_mode,
                    self.cancel,
                    Some(scope),
                    GeometryCheck::Profile(profile),
                    required_min_ppi,
                )?;
                Ok(raw)
            })();
            match validation {
                Ok(raw) => {
                    if !step_notes.is_empty() {
                        self.diagnostics(step_notes)?;
                    }
                    self.cache_plan(cache_key.as_deref(), &raw)?;
                    return Ok(raw);
                }
                Err(error) => {
                    let mut diag = step_notes;
                    diag.push(json!({"kind": "templateContentValidation", "attempt": attempt, "error": error}));
                    self.diagnostics(diag)?;
                    if attempt == repairs {
                        return Err(error);
                    }
                    self.progress(&format!("Repairing template page content before purchasing images (attempt {})", attempt + 1), 0.48)?;
                    generation.prompt = template_repair_prompt(&original_prompt, &response.text, &error)?;
                }
            }
        }
        Err("Template content planning exhausted bounded repair attempts".into())
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn template_review_group(
        &mut self,
        request: TaskRequest,
        context: Value,
        style: &SkillSnapshot,
        base: DocState,
        prefs: Prefs,
        review: BatchScope,
        total_pages: u32,
    ) -> Result<Product, String> {
        let required_min_ppi = self.print.as_ref().and_then(|contract| contract.min_image_ppi());
        let min_ppi = required_min_ppi.unwrap_or(150.0);
        let end = review_end(review)?;
        if usize::try_from(end).map_err(|_| "Invalid review page range")? > base.doc.spreads.len() {
            return Err("Review page range exceeds the native document".into());
        }
        let capacity = crate::template_plan::capacity(style, request.settings.output_mode)?;
        if capacity == 0 {
            return Err("Native template has no safe internal page capacity".into());
        }
        let mut scratch = Session::new();
        scratch.prefs = prefs.clone();
        scratch.add_document(base.clone());
        let mut pending = Vec::new();
        let mut plan_bytes = 0usize;
        let mut next = review.start;
        while next < end {
            check_cancel(self.cancel)?;
            let scope = BatchScope { start: next, count: capacity.min(end - next) };
            let mut chunk_context = internal_template_context(&context, scratch.doc().map_err(|e| e.to_string())?, review, scope)?;
            chunk_context
                .as_object_mut()
                .ok_or("Invalid internal template context")?
                .insert("curriculum_blueprint".into(), crate::template_plan::curriculum_blueprint(&request.prompt, style, scope, total_pages)?);
            // Only the complete group creates proof artifacts and checkpoints.
            // All parts share the actual ledger, cancellation and pinned keys.
            let remaining_chunks = (end - next).div_ceil(capacity);
            let plan = self.template_content_with_min_requests(
                &request,
                &chunk_context,
                style,
                scratch.doc().map_err(|e| e.to_string())?,
                &prefs,
                scope,
                remaining_chunks,
            )?;
            let bytes = serde_json::to_vec(&plan).map_err(|e| e.to_string())?.len();
            plan_bytes =
                plan_bytes.checked_add(bytes).filter(|total| *total <= MAX_GROUP_PLAN_BYTES).ok_or("Review plans exceed the 256-MiB limit")?;
            if pending.len() >= usize::try_from(MAX_REVIEW_PAGES).map_err(|_| "Invalid review plan limit")? {
                return Err("Review plan count exceeds its bounded page limit".into());
            }
            scratch.execute("production.apply", &json!({"commands":plan.commands,"outputMode":request.settings.output_mode,"label":plan.title,"requirePreflight":true,"minPpi":min_ppi}))
                .map_err(|e| e.to_string())?;
            pending.push((plan, scope));
            next = next.checked_add(scope.count).ok_or("Internal template page range overflow")?;
        }
        validate_batch_result(&base, scratch.doc().map_err(|e| e.to_string())?, review)?;
        // Predictable request/token/cost failures for the ENTIRE image phase must
        // stop before its first purchase. Admission uses a clone, not the ledger
        // that records real provider usage. Internal parts have separate caches.
        let profile = style.design_profile.as_ref().ok_or("Missing pinned design profile")?;
        let style_context = illustration_style(Some(profile))?;
        let mut projected = self.ledger.clone();
        let mut cached = BTreeMap::new();
        let mut cached_bytes = 0usize;
        for (plan, _) in &pending {
            check_cancel(self.cancel)?;
            for image in &plan.images {
                self.print_image_size(image, Some(profile), &request.settings.image_model)?;
            }
            self.preload_images(plan, &request.settings, Some(profile), &style_context, &mut cached, &mut cached_bytes)?;
            project_image_requests(&mut projected, plan, &request.settings, Some(profile), &style_context, &cached)?;
        }
        // Rebuild once with the actual artwork; the text-only draft never reaches
        // the live document and is released before allocating the illustrated one.
        drop(scratch);
        let mut scratch = Session::new();
        scratch.prefs = prefs.clone();
        scratch.add_document(base.clone());
        let mut plans = Vec::new();
        plan_bytes = 0;
        for (plan, scope) in pending {
            check_cancel(self.cancel)?;
            let plan = self.expand_images(plan, &request.settings, &mut cached, &mut cached_bytes, Some(profile))?;
            self.audit_print(&plan, scratch.doc().map_err(|e| e.to_string())?, &prefs)?;
            simulate_styled_with_ppi(
                &plan,
                scratch.doc().map_err(|e| e.to_string())?,
                &prefs,
                request.settings.output_mode,
                self.cancel,
                Some(scope),
                GeometryCheck::Profile(profile),
                required_min_ppi,
            )?;
            let bytes = serde_json::to_vec(&plan).map_err(|e| e.to_string())?.len();
            plan_bytes =
                plan_bytes.checked_add(bytes).filter(|total| *total <= MAX_GROUP_PLAN_BYTES).ok_or("Review plans exceed the 256-MiB limit")?;
            scratch.execute("production.apply", &json!({"commands":plan.commands,"outputMode":request.settings.output_mode,"label":plan.title,"requirePreflight":true,"minPpi":min_ppi}))
                .map_err(|e| e.to_string())?;
            plans.push(plan);
        }
        validate_batch_result(&base, scratch.doc().map_err(|e| e.to_string())?, review)?;
        self.progress("Preparing complete review proofs from all internal page parts", 0.9)?;
        let proofs = review_proofs(&mut scratch, self.cancel, review, &request.settings, &request.prompt)?;
        Ok(Product::BatchGroup { plans, proofs })
    }
}
