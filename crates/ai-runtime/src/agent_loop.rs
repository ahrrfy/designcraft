//! Multi-turn tool-calling agent loop for open-document understanding, inspection, and relational execution.

use crate::doc_tools::{DeferredImagePlan, DocToolWorkspace, tool_definitions};
use crate::worker::{Product, Runner};
use designcraft_ai_providers::{AgentMessage, AgentTurnRequest, ToolCall, ToolResultMessage, validate_endpoint};
use designcraft_ai_skills::SkillSnapshot;
use designcraft_ai_types::protocol::{AiSettings, ProductionPlan, TaskRequest};
use designcraft_ai_types::{EngineAction, OutputMode, ProviderKind, prompt_explicitly_requests_image_generation};
use designcraft_engine::{DocState, Prefs};
use serde_json::{Value, json};

const MAX_AGENT_TURNS: usize = 20;

pub(crate) fn has_configured_image_model(settings: &AiSettings) -> bool {
    if validate_endpoint(&settings.image_model).is_err() {
        return false;
    }
    match settings.image_model.provider {
        ProviderKind::ComfyUi => settings.comfy_workflow.is_some(),
        ProviderKind::Ollama | ProviderKind::LocalOpenAi => false,
        ProviderKind::Gemini | ProviderKind::OpenAi => {
            let model = settings.image_model.model.trim();
            !model.is_empty() && !designcraft_ai_types::is_non_image_model(settings.image_model.provider, model)
        }
    }
}

pub(crate) fn should_auto_execute_queued_images(prompt: &str, settings: &AiSettings) -> bool {
    has_configured_image_model(settings)
        && (settings.image_model.provider == ProviderKind::ComfyUi || prompt_explicitly_requests_image_generation(prompt))
}

pub(crate) fn effective_open_doc_image_settings(settings: &AiSettings) -> AiSettings {
    let mut effective = settings.clone();
    let started_native_only = effective.output_mode == OutputMode::NativeOnly;
    effective.output_mode = OutputMode::NativeWithImages;
    if started_native_only && !designcraft_ai_types::supports_gemini_image_review(effective.text_model.provider, &effective.text_model.model) {
        effective.verify_generated_images = false;
    }
    effective
}

pub(crate) fn run_open_document_agent_loop(
    runner: &mut Runner<'_>,
    request: &TaskRequest,
    context: &Value,
    catalog: &Value,
    skills: &[SkillSnapshot],
    base: &DocState,
    prefs: &Prefs,
) -> Result<Product, String> {
    runner.progress("Inspecting open document structure and typography", 0.10)?;

    let min_ppi = runner.required_min_ppi().unwrap_or(150.0);
    let allow_image_queue = has_configured_image_model(&request.settings);
    let supports_vision = designcraft_ai_providers::supports_agent_vision(&request.settings.text_model);
    let tools = tool_definitions(allow_image_queue);
    let mut workspace = DocToolWorkspace::new(base, prefs, catalog.clone(), min_ppi);
    workspace.allow_image_queue = allow_image_queue;
    workspace.supports_vision = supports_vision;
    workspace.ai_root = context.get("_ai_profile_root").and_then(Value::as_str).map(std::path::PathBuf::from);
    let overview = workspace.inspect_document_overview(request)?;

    let system = build_agent_system_prompt(catalog, skills, allow_image_queue, supports_vision)?;
    let initial_user = build_initial_user_message(request, context, &overview)?;
    let mut messages = vec![AgentMessage::User { text: initial_user, images: Vec::new() }];

    let style_profile = skills.iter().find_map(|s| s.design_profile.as_ref());

    for turn_idx in 0..MAX_AGENT_TURNS {
        let remaining_before = runner.remaining_requests();
        if remaining_before == 0 {
            break;
        }
        let progress_frac = (0.15 + (turn_idx as f32 * 0.04)).min(0.82);
        runner.progress(&format!("Analyzing document and executing native engine tools (step {})", turn_idx.saturating_add(1)), progress_frac)?;

        prune_historical_tool_images(&mut messages);
        let turn_req = AgentTurnRequest {
            system: system.clone(),
            messages: messages.clone(),
            tools: tools.clone(),
            max_output_tokens: request.settings.budget.max_output_tokens,
            resources: request.settings.resources.clone(),
        };
        let turn = runner.agent_turn(&request.settings.text_model, &turn_req)?;
        let remaining_after = runner.remaining_requests();

        if !turn.tool_calls.is_empty() {
            messages.push(AgentMessage::Assistant {
                text: turn.text.clone(),
                tool_calls: turn.tool_calls.clone(),
                raw_provider_turn: turn.raw_provider_turn.clone(),
            });
            let user_bytes: usize = messages
                .iter()
                .filter_map(|m| match m {
                    AgentMessage::User { images, .. } => Some(images.iter().map(|i| i.bytes.len()).sum::<usize>()),
                    _ => None,
                })
                .sum();
            workspace.visual_images_in_turn = 0;
            workspace.visual_bytes_in_turn = 0;
            workspace.max_visual_bytes_per_turn = (2 * 1024 * 1024usize).saturating_sub(user_bytes).saturating_sub(64 * 1024);
            let mut results = Vec::with_capacity(turn.tool_calls.len());
            for call in &turn.tool_calls {
                let (result_json, images, _is_error) = workspace.execute_tool_multimodal(&call.name, &call.arguments, request, remaining_after);
                results.push(ToolResultMessage { call_id: call.id.clone(), name: call.name.clone(), output: result_json, images });
            }
            messages.push(AgentMessage::ToolResults { results });
            if workspace.completed_title.is_some() {
                break;
            }
            continue;
        }

        // Fallback for local models that emit a direct JSON plan `{title, commands, images?}` without tool_calls.
        if let Some(fallback_outcome) = try_apply_json_fallback(&turn.text, &mut workspace, request, remaining_after, allow_image_queue) {
            match fallback_outcome {
                Ok(()) => break,
                Err(validation_err) => {
                    messages.push(AgentMessage::Assistant { text: turn.text, tool_calls: Vec::new(), raw_provider_turn: turn.raw_provider_turn });
                    messages.push(AgentMessage::User {
                        text: format!(
                            "Your JSON command plan failed native validation: {validation_err}\nCall `inspect_pages` / `search_document` if you need exact IDs, then call `apply_engine_commands` and `complete_task`."
                        ),
                        images: Vec::new(),
                    });
                    continue;
                }
            }
        }

        if workspace.completed_title.is_some() {
            break;
        }

        messages.push(AgentMessage::Assistant { text: turn.text, tool_calls: Vec::new(), raw_provider_turn: turn.raw_provider_turn });
        messages.push(AgentMessage::User {
            text: "Do not reply with prose only. Invoke the native document tools (`inspect_pages`, `search_document`, `inspect_graphics_and_captions`, `render_visual_check`, `apply_engine_commands`, `queue_image_generations`) to inspect and apply all requested edits, then call `complete_task`.".into(),
            images: Vec::new(),
        });
    }

    // If the model ran out of turns after applying edits without calling `complete_task`, rescue only if preflight has no blocking errors.
    if workspace.completed_title.is_none() {
        if workspace.applied_chunks.is_empty() && workspace.deferred_images.is_empty() {
            return Err("AI agent finished without applying any native document commands or queueing illustrations".into());
        }
        let mut preflight = workspace.preflight_summary()?;
        if workspace.has_blocking_preflight(&preflight) {
            let _ = workspace.execute_tool(
                "apply_engine_commands",
                &json!({"commands": [{"command": "story.fitOverset", "params": {"minSize": 8.0}}]}),
                request,
                0,
            );
            preflight = workspace.preflight_summary()?;
        }
        if workspace.has_blocking_preflight(&preflight) {
            let errors = preflight.get("errors").and_then(Value::as_u64).unwrap_or(1);
            return Err(format!(
                "Edited document still has {errors} blocking preflight error(s) after exhausting turn budget; changes were not published"
            ));
        }
        workspace.completed_title = Some(default_title_from_prompt(&request.prompt));
        workspace.completed_summary = Some("Applied verified native document edits".into());
    }

    let title = workspace.completed_title.unwrap_or_else(|| default_title_from_prompt(&request.prompt));
    let mut summary = workspace.completed_summary.unwrap_or_else(|| title.clone());
    let mut chunks = workspace.applied_chunks;
    let mut deferred_batches = workspace.deferred_images;

    if !deferred_batches.is_empty() {
        let total_imgs: usize = deferred_batches.iter().map(|b| b.images.len()).sum();
        if should_auto_execute_queued_images(&request.prompt, &request.settings) {
            let effective_img_settings = effective_open_doc_image_settings(&request.settings);
            runner.extend_local_comfy_request_allowance(&effective_img_settings, total_imgs)?;
            match runner.can_admit_deferred_image_batches(&title, &deferred_batches, &effective_img_settings, style_profile) {
                Ok(()) => {
                    runner.progress("Generating single-subject illustrations and placing into native frames", 0.86)?;
                    let expanded_chunks = runner.expand_deferred_image_batches(
                        &title,
                        &deferred_batches,
                        &effective_img_settings,
                        style_profile,
                        &mut workspace.session,
                        min_ppi,
                    )?;
                    chunks.extend(expanded_chunks);
                    deferred_batches.clear();
                }
                Err(budget_reason) if !chunks.is_empty() => {
                    summary = format!("{summary} ({total_imgs} illustration(s) staged for confirmation — {budget_reason})");
                }
                Err(err) => return Err(err),
            }
        } else {
            summary = format!("{summary} ({total_imgs} illustration(s) queued and awaiting confirmation before paid image generation)");
        }
    }

    if chunks.is_empty() && deferred_batches.is_empty() {
        return Err("No native document actions or queued illustrations were produced".into());
    }

    runner.progress("Finalizing verified native document update", 0.96)?;
    Ok(Product::Agentic { title, summary, chunks, deferred_images: deferred_batches })
}

/// Retain raw visual render bytes only on recent `ToolResults` turns within the 8-image / 2 MiB
/// provider cap while preserving structured JSON output across all turns.
pub(crate) fn prune_historical_tool_images(messages: &mut [AgentMessage]) {
    let mut user_imgs = 0usize;
    let mut user_bytes = 0usize;
    for msg in messages.iter() {
        if let AgentMessage::User { images, .. } = msg {
            user_imgs = user_imgs.saturating_add(images.len());
            for img in images {
                user_bytes = user_bytes.saturating_add(img.bytes.len());
            }
        }
    }
    let max_tool_imgs = 8usize.saturating_sub(user_imgs).min(4);
    let max_tool_bytes = (2 * 1024 * 1024usize).saturating_sub(user_bytes).saturating_sub(64 * 1024);

    let mut used_imgs = 0usize;
    let mut used_bytes = 0usize;
    let mut tool_turns_seen = 0usize;
    for msg in messages.iter_mut().rev() {
        if let AgentMessage::ToolResults { results } = msg {
            let allow_turn = tool_turns_seen == 0;
            tool_turns_seen = tool_turns_seen.saturating_add(1);
            for res in results.iter_mut() {
                if res.images.is_empty() {
                    continue;
                }
                if !allow_turn {
                    res.images.clear();
                    continue;
                }
                let before_len = res.images.len();
                let mut kept = Vec::with_capacity(before_len);
                for img in res.images.drain(..) {
                    if used_imgs < max_tool_imgs && used_bytes.saturating_add(img.bytes.len()) <= max_tool_bytes {
                        used_imgs = used_imgs.saturating_add(1);
                        used_bytes = used_bytes.saturating_add(img.bytes.len());
                        kept.push(img);
                    }
                }
                res.images = kept;
                if res.images.len() != before_len
                    && let Some(obj) = res.output.as_object_mut()
                {
                    obj.insert("visionAttached".into(), serde_json::json!(!res.images.is_empty()));
                    if obj.contains_key("visionAttachedCount") {
                        obj.insert("visionAttachedCount".into(), serde_json::json!(res.images.len()));
                    }
                }
            }
        }
    }
}

fn build_agent_system_prompt(catalog: &Value, skills: &[SkillSnapshot], allow_image_queue: bool, supports_vision: bool) -> Result<String, String> {
    let (compact_cat, skill_instructions) = crate::worker::compact_catalog_and_skills_for_agent(catalog, skills)?;
    let image_tool_guidance = if allow_image_queue {
        "5. GRAPHIC-CAPTION ALIGNMENT & COMPOSITE/DAMAGED IMAGES:\n\
         - Call `inspect_graphics_and_captions` whenever the prompt mentions images, pictures, cards, captions, mismatches, or whole-book review.\n\
         - On RTL documents (`rightToLeftBinding: true`), horizontal reading order is RIGHT-TO-LEFT (rightmost X = 1st card, leftmost X = last card). Check that every caption's X coordinate aligns under/over its matching graphic card.\n\
         - Bind captions relationally to their graphic frames via `object.altText` (`{\"ids\": [graphicId], \"text\": captionText}`) and `object.caption` (`{\"ids\": [graphicId], \"text\": \"{altText}\", \"position\": \"below\", \"live\": true, \"group\": true}`) so the graphic and live caption move together as one atomic unit.\n\
         - When `inspect_graphics_and_captions` flags `compositeStripCandidate: true` (multiple captions under a single wide multi-subject strip raster) or when an illustration is baked/corrupted/mismatched and needs regeneration: DO NOT delete the old graphic frame inside `apply_engine_commands`! Instead call `queue_image_generations` with `companion_commands` (deleting the old strip frame and old detached caption frames if replacing them, creating individual 1:1 graphic frames at `suggestedGraphicRect`, setting `object.altText` and `object.caption` with `live: true, group: true`) and one single-subject `images` entry per card (pure illustration of ONE subject, ZERO text, letters, or numbers inside the artwork)."
    } else {
        "5. GRAPHIC-CAPTION ALIGNMENT (NO IMAGE MODEL CONFIGURED):\n\
         - Call `inspect_graphics_and_captions` when checking word-picture alignment. On RTL documents, reading order is RIGHT-TO-LEFT (rightmost X = 1st item). Align captions, fix mismatched labels, set `object.altText`, and bind grouped live captions with `object.caption` (`live: true, group: true`)."
    };
    let vision_guidance = if supports_vision {
        "- VISUAL PROOFING (`render_visual_check`): Call `render_visual_check` (`{\"page\": 1}` or cropped `{\"page\": 1, \"rect\": [x0, y0, x1, y1]}`) to visually inspect actual rendered typography, Arabic shaping/diacritics, RTL reading alignment, running headers/folios, image crops, spacing, and visual hierarchy before finalizing."
    } else {
        "- INK & GEOMETRY PROOFING (`render_visual_check`): Call `render_visual_check` when needed to verify rendered pixel ink bounds, non-paper coverage ratio, and preflight state on a page or cropped region."
    };

    Ok(format!(
        "You are DesignCraft's native layout and publishing engine agent.\n\
         You understand free-form user prompts in Arabic, English, or any language and execute edits directly on the open DesignCraft document through native engine tools.\n\
         \n\
         CORE ENGINEERING & PUBLISHING RULES:\n\
         1. INSPECT BEFORE EDITING:\n\
         - Turn 1 already includes `inspect_document_overview` (page geometry, facing-page sides, parent spreads, sections, text variables, special live markers, styles, swatches, installed Arabic fonts, and preflight).\n\
         - For page-level content, call `inspect_pages` (up to 16 pages per call; emit multiple `inspect_pages` tool calls in parallel in a single turn to inspect larger books quickly), `search_document` (plain text or regex across both document spreads and parent spreads), `inspect_graphics_and_captions`, and `render_visual_check`.\n\
         {vision_guidance}\n\
         2. ARABIC TYPOGRAPHY, TASHKEEL & FONTS:\n\
         - To strip or lighten Arabic diacritics (tashkeel) across the whole document or a page range while preserving letters, punctuation, variables, shadda, and formatting runs, call `apply_engine_commands` with `{{\"command\": \"type.diacritics\", \"params\": {{\"mode\": \"none\" | \"light\" | \"keep\", \"pages\": [start1Based, end1Based]}}}}` (omit `pages` for the whole document including parent spreads).\n\
         - To change the Arabic font across the book (stories + paragraph styles + character styles) without disturbing Latin/math fonts or bold/italic weights, use `{{\"command\": \"font.replace\", \"params\": {{\"script\": \"arabic\", \"toFamily\": \"<exact family from availableArabicFonts or auto>\", \"redefineStyles\": true}}}}`.\n\
         3. RELATIONAL MASTER PAGES, SECTIONS, VARIABLES & FOLIOS:\n\
         - Prefer relational document primitives (`variables.define` with `\"type\": \"runningHeader\"` or `\"custom\"`, `layout.parents.new` with `\"prefix\"` & `\"name\"`, `layout.pages.insert`, `layout.pages.applyParent`, `layout.section`, `layout.chapterNumbering`) over disconnected ad-hoc boxes whenever establishing running headers or page numbering.\n\
         - To place live folios or running headers on a master/parent spread, pass `\"spread\": {{\"kind\": \"parent\", \"index\": 0}}` to `frame.create` (`\"content\": \"text\"`) using `specialMarkers.currentPageNumber` (`\"\\u{{E000}}\"`), `specialMarkers.sectionMarker` (`\"\\u{{E001}}\"`), or the `char` field from `textVariables`.\n\
         - On facing-page spreads, ALWAYS check each page's `side` (`left` / `right`), `outerEdge`, and `safeRectInSpread` `[x0, y0, x1, y1]`. Keep all headers, footers, badges, and content strictly inside `safeRectInSpread` and away from `spineX`.\n\
         - When verifying or fixing unit numbers, chapter headers, and page folios across a book, inspect all pages sequentially so unit progression and page numbers are 100% consistent with each page's actual content and `doc.page_name`.\n\
         - CRITICAL `find.change` rule: NEVER omit `change` in `find.change` (omitting `change` replaces matches with empty string!). Pass `\"change\": \"$0\"` when styling matches without changing text, or use `story.setText` / `story.replaceRange` for targeted story edits.\n\
         4. ATOMIC EXECUTION & ZERO OVERSET:\n\
         - Every `apply_engine_commands` batch runs on a private draft and returns created IDs plus live `preflight` (`errors`, `oversetErrors`, `warnings`, `issues`).\n\
         - Within a single `apply_engine_commands` batch, reference earlier results via `$N.id` or `$N.story` (0-based within that batch).\n\
         - Before calling `complete_task`, `preflight.oversetErrors` MUST be `0`. If font replacement or text edits cause overset text, run `{{\"command\": \"story.fitOverset\", \"params\": {{\"minSize\": 8.0}}}}` or enlarge/reflow the frame inside `safeRectInSpread`.\n\
         {image_tool_guidance}\n\
         6. KNOWLEDGE RAG, PREFLIGHT, DATA RECORDS & MULTI-FORMAT ADAPTATION:\n\
         - Call `search_project_knowledge` whenever user attachments or active Knowledge Packs contain source copy, brand rules, or curriculum facts.\n\
         - Call `lookup_engine_commands` to inspect exact command parameter schemas before using unfamiliar engine commands.\n\
         - Call `inspect_preflight` anytime to verify zero overset/missing-glyph/bleed errors.\n\
         - Call `populate_data_records` when an attached `.xlsx`/`.csv`/`.tsv` (`DataRecords`) should populate repeated cards, badges, menus, or catalog items.\n\
         - Call `adapt_layout_formats` when adapting a campaign across multiple social/print formats (`AlternateLayouts` or `SeparateDocuments`).\n\
         7. COMPLETION:\n\
         - Once every requirement in the user's prompt is satisfied and there is zero overset text, call `complete_task` with a clear `title` and `summary` (in the user's prompt language). All applied batches commit to the user's open document as ONE atomic Undo step.\n\
         \n\
         VETTED NATIVE ENGINE COMMAND CATALOG:\n\
         {compact_cat}\n\
         \n\
         ACTIVE SKILLS & DESIGN GUIDANCE:\n\
         {skill_instructions}"
    ))
}

fn build_initial_user_message(request: &TaskRequest, context: &Value, overview: &Value) -> Result<String, String> {
    let resolved_fonts = context.get("resolved_print_fonts").cloned().unwrap_or(Value::Null);
    let art_direction = context.get("brief_art_direction").cloned().unwrap_or(Value::Null);
    let selection_ctx = context.get("selection_context").cloned().unwrap_or(Value::Null);
    let design_memory = context.get("session_design_memory").cloned().unwrap_or(Value::Null);
    let rag_hits = context.get("rag_grounding_hits").cloned().unwrap_or(Value::Null);
    let payload = json!({
        "userPrompt": request.prompt,
        "resolvedPrintFonts": resolved_fonts,
        "artDirection": art_direction,
        "selectionContext": selection_ctx,
        "sessionDesignMemory": design_memory,
        "initialKnowledgeHits": rag_hits,
        "attachments": request.attachments,
        "targetFormats": request.target_formats,
        "documentOverview": overview,
    });
    serde_json::to_string(&payload).map_err(|e| e.to_string())
}

fn try_apply_json_fallback(
    text: &str,
    workspace: &mut DocToolWorkspace,
    request: &TaskRequest,
    remaining_turns: u32,
    allow_image_queue: bool,
) -> Option<Result<(), String>> {
    let raw_json = extract_json_object(text)?;
    let parsed: Value = serde_json::from_str(raw_json).ok()?;
    let obj = parsed.as_object()?;
    let commands = obj.get("commands")?;
    if !commands.is_array() {
        return None;
    }
    let title = obj
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default_title_from_prompt(&request.prompt));

    let saved_doc = workspace.session.doc().ok().cloned();
    let saved_chunks_len = workspace.applied_chunks.len();
    let saved_images_len = workspace.deferred_images.len();

    let rollback = |ws: &mut DocToolWorkspace, doc_opt: Option<DocState>| {
        ws.applied_chunks.truncate(saved_chunks_len);
        ws.deferred_images.truncate(saved_images_len);
        ws.completed_title = None;
        ws.completed_summary = None;
        if let Some(prev_doc) = doc_opt {
            let mut restored = designcraft_engine::Session::new();
            restored.prefs = ws.session.prefs.clone();
            restored.add_document(prev_doc);
            ws.session = restored;
        }
    };

    if commands.as_array().is_some_and(|arr| !arr.is_empty()) {
        let (res, is_err) =
            workspace.execute_tool("apply_engine_commands", &json!({"commands": commands, "summary": title}), request, remaining_turns);
        if is_err {
            let msg = res.get("error").and_then(Value::as_str).unwrap_or("invalid commands").to_string();
            rollback(workspace, saved_doc);
            return Some(Err(msg));
        }
    }

    if allow_image_queue
        && let Some(images) = obj.get("images").and_then(Value::as_array)
        && !images.is_empty()
    {
        let converted: Vec<Value> = images
            .iter()
            .map(|img| {
                json!({
                    "spread": img.get("spread").or_else(|| img.get("page")).cloned().unwrap_or(json!(0)),
                    "rect": img.get("rect").cloned().unwrap_or(json!([0, 0, 0, 0])),
                    "prompt": img.get("prompt").cloned().unwrap_or(json!("")),
                    "label": img.get("label").cloned().unwrap_or(json!("Illustration")),
                })
            })
            .collect();
        let mut queue_args = json!({"summary": title, "images": converted});
        if let Some(companion) = obj.get("companion_commands").filter(|v| v.is_array())
            && let Some(qobj) = queue_args.as_object_mut()
        {
            qobj.insert("companion_commands".into(), companion.clone());
        }
        let (res, is_err) = workspace.execute_tool("queue_image_generations", &queue_args, request, remaining_turns);
        if is_err {
            let msg = res.get("error").and_then(Value::as_str).unwrap_or("invalid images").to_string();
            rollback(workspace, saved_doc);
            return Some(Err(msg));
        }
    }

    let (done_res, done_err) = workspace.execute_tool("complete_task", &json!({"title": title, "summary": title}), request, remaining_turns);
    if done_err {
        let msg = done_res.get("error").and_then(Value::as_str).unwrap_or("preflight error").to_string();
        rollback(workspace, saved_doc);
        return Some(Err(msg));
    }
    Some(Ok(()))
}

fn extract_json_object(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed);
    }
    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    if end > start { trimmed.get(start..=end) } else { None }
}

fn default_title_from_prompt(prompt: &str) -> String {
    let clean = crate::brief_planner::sanitize_visible_title(prompt);
    if clean.trim().is_empty() { "AI Document Edit".into() } else { clean }
}

#[allow(dead_code)]
fn _assert_types(_: &ToolCall, _: &ProductionPlan, _: &EngineAction, _: &DeferredImagePlan) {}
