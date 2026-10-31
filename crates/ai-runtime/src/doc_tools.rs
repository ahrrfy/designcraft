//! Native on-demand document inspection and execution tools for multi-turn open-document AI loops.

use designcraft_ai_providers::{ImageInput, ToolDefinition};
use designcraft_ai_types::protocol::{ImagePlacement, TaskRequest};
use designcraft_ai_types::{EngineAction, OutputMode};
use designcraft_engine::doc::{Content, Item, ItemId, PageSide, Shape, SpreadRef, StoryId, vars::var_char};
use designcraft_engine::{DocState, Prefs, Session};
use designcraft_fonts::FontDb;
use designcraft_render::{Placed, RenderOptions, Renderer};
use serde_json::{Value, json};

const MAX_TOOL_PAGES_PER_CALL: usize = 16;
const MAX_STORY_TOOL_BYTES: usize = 16 * 1024;
const MAX_CHUNKS: usize = 64;
const MAX_ACTIONS: usize = 512;
const MAX_IMAGES: usize = 8;
const MAX_IMAGE_BATCHES: usize = 4;
const MAX_VISUAL_IMAGES_PER_TURN: usize = 4;
const DEFAULT_MAX_VISUAL_BYTES_PER_TURN: usize = (2 * 1024 * 1024) - (64 * 1024);

#[derive(Clone, Debug, Default)]
pub(crate) struct DeferredImagePlan {
    pub companion_commands: Vec<EngineAction>,
    pub images: Vec<ImagePlacement>,
    pub summary: String,
}

pub(crate) struct DocToolWorkspace {
    pub session: Session,
    pub catalog: Value,
    pub min_ppi: f64,
    pub allow_image_queue: bool,
    pub supports_vision: bool,
    pub visual_images_in_turn: usize,
    pub visual_bytes_in_turn: usize,
    pub max_visual_bytes_per_turn: usize,
    pub renderer: Renderer,
    pub applied_chunks: Vec<Vec<EngineAction>>,
    pub deferred_images: Vec<DeferredImagePlan>,
    pub completed_title: Option<String>,
    pub completed_summary: Option<String>,
}

impl DocToolWorkspace {
    pub(crate) fn new(base: &DocState, prefs: &Prefs, catalog: Value, min_ppi: f64) -> Self {
        let mut session = Session::new();
        session.prefs = prefs.clone();
        session.add_document(base.clone());
        Self {
            session,
            catalog,
            min_ppi,
            allow_image_queue: true,
            supports_vision: true,
            visual_images_in_turn: 0,
            visual_bytes_in_turn: 0,
            max_visual_bytes_per_turn: DEFAULT_MAX_VISUAL_BYTES_PER_TURN,
            renderer: Renderer::new(),
            applied_chunks: Vec::new(),
            deferred_images: Vec::new(),
            completed_title: None,
            completed_summary: None,
        }
    }

    pub(crate) fn total_queued_images(&self) -> usize {
        self.deferred_images.iter().map(|b| b.images.len()).sum()
    }

    pub(crate) fn total_applied_actions(&self) -> usize {
        self.applied_chunks.iter().map(Vec::len).sum()
    }

    pub(crate) fn execute_tool(&mut self, name: &str, args: &Value, request: &TaskRequest, remaining_turns: u32) -> (Value, bool) {
        let (payload, _images, is_err) = self.execute_tool_multimodal(name, args, request, remaining_turns);
        (payload, is_err)
    }

    pub(crate) fn execute_tool_multimodal(
        &mut self,
        name: &str,
        args: &Value,
        request: &TaskRequest,
        remaining_turns: u32,
    ) -> (Value, Vec<ImageInput>, bool) {
        let result: Result<(Value, Vec<ImageInput>), String> = match name {
            "inspect_document_overview" => self.inspect_document_overview(request).map(|v| (v, Vec::new())),
            "inspect_pages" => self.inspect_pages(args).map(|v| (v, Vec::new())),
            "search_document" => self.search_document(args).map(|v| (v, Vec::new())),
            "inspect_graphics_and_captions" => self.inspect_graphics_and_captions(args).map(|v| (v, Vec::new())),
            "render_visual_check" => self.render_visual_check(args),
            "apply_engine_commands" => self.apply_engine_commands(args).map(|v| (v, Vec::new())),
            "queue_image_generations" => self.queue_image_generations(args).map(|v| (v, Vec::new())),
            "complete_task" => self.complete_task(args, remaining_turns).map(|v| (v, Vec::new())),
            other => {
                let available = if self.allow_image_queue {
                    "inspect_document_overview, inspect_pages, search_document, inspect_graphics_and_captions, render_visual_check, apply_engine_commands, queue_image_generations, complete_task"
                } else {
                    "inspect_document_overview, inspect_pages, search_document, inspect_graphics_and_captions, render_visual_check, apply_engine_commands, complete_task"
                };
                Err(format!("Unknown tool `{other}`. Available tools: {available}."))
            }
        };
        match result {
            Ok((mut payload, images)) => {
                if let Some(obj) = payload.as_object_mut() {
                    obj.insert("remainingRequests".into(), json!(remaining_turns));
                    if remaining_turns <= 1 && self.completed_title.is_none() {
                        obj.insert(
                            "budgetNotice".into(),
                            json!("Only 1 model turn remains in max_requests; finalize edits and call complete_task in your next response."),
                        );
                    }
                }
                (payload, images, false)
            }
            Err(err) => (
                json!({
                    "ok": false,
                    "error": truncate_utf8(&err, 4096),
                    "remainingRequests": remaining_turns,
                }),
                Vec::new(),
                true,
            ),
        }
    }

    pub(crate) fn inspect_document_overview(&mut self, request: &TaskRequest) -> Result<Value, String> {
        let preflight = self.preflight_summary()?;
        let state = self.session.doc().map_err(|e| e.to_string())?;
        let doc = &state.doc;
        let scoped_fonts = FontDb::global().scoped(doc.font_scope);
        let arabic_fonts = scoped_fonts.available_arabic_families();
        let preferred_arabic = scoped_fonts.preferred_arabic_font().map(|m| m.regular_family);

        let mut page_cursor = 0usize;
        let mut spreads_out = Vec::with_capacity(doc.spreads.len());
        for (spread_idx, spread) in doc.spreads.iter().enumerate() {
            let mut pages_out = Vec::with_capacity(spread.pages.len());
            let spine_x = if spread.pages.len() >= 2 { spread.pages.first().map(|p| p.x + p.width) } else { None };
            for page in &spread.pages {
                let page_1based = page_cursor.saturating_add(1);
                let name = doc.page_name(page_cursor);
                let safe = page.margin_rect();
                let side_str = match page.side {
                    PageSide::Left => "left",
                    PageSide::Right => "right",
                    PageSide::Single => "single",
                };
                let outer_edge = outer_edge_side(doc.settings.facing_pages, doc.settings.right_to_left_binding, page.side);
                let parent_prefix =
                    page.parent.and_then(|pid| doc.parents.iter().find(|p| p.id == pid)).and_then(|p| p.parent.as_ref()).map(|m| m.prefix.clone());
                pages_out.push(json!({
                    "page": page_1based,
                    "docPageIndex": page_cursor,
                    "name": name,
                    "side": side_str,
                    "outerEdge": outer_edge,
                    "parent": parent_prefix,
                    "x": page.x,
                    "width": page.width,
                    "height": page.height,
                    "margins": page.margins,
                    "safeRectInSpread": [safe.x0, safe.y0, safe.x1, safe.y1],
                }));
                page_cursor = page_cursor.saturating_add(1);
            }
            spreads_out.push(json!({
                "spread": spread_idx,
                "spineX": spine_x,
                "pages": pages_out,
                "itemCount": spread.items.len(),
            }));
        }

        let parents_out: Vec<Value> = doc
            .parents
            .iter()
            .enumerate()
            .map(|(idx, p)| {
                let items: Vec<Value> = p
                    .items
                    .iter()
                    .take(64)
                    .map(|it| {
                        let b = it.bounds();
                        let text = it.text_frame().and_then(|tf| doc.story(tf.story)).map(|st| truncate_utf8(&st.text, 256));
                        json!({
                            "id": it.id.0,
                            "kind": item_kind_name(it),
                            "rect": [b.x0, b.y0, b.x1, b.y1],
                            "label": it.label,
                            "story": it.text_frame().map(|tf| tf.story.0),
                            "text": text,
                        })
                    })
                    .collect();
                json!({
                    "index": idx,
                    "id": p.id.0,
                    "prefix": p.parent.as_ref().map(|m| m.prefix.as_str()).unwrap_or(""),
                    "name": p.parent.as_ref().map(|m| m.name.as_str()).unwrap_or(""),
                    "pages": p.pages.len(),
                    "itemCount": p.items.len(),
                    "items": items,
                })
            })
            .collect();

        let variables_out: Vec<Value> = doc
            .text_variables
            .iter()
            .enumerate()
            .filter_map(|(i, v)| {
                var_char(i).map(|ch| {
                    json!({
                        "index": i,
                        "name": v.name,
                        "kind": v.kind,
                        "char": ch.to_string(),
                    })
                })
            })
            .collect();

        let para_styles: Vec<Value> = doc
            .styles
            .paragraph
            .iter()
            .take(64)
            .map(|s| {
                json!({
                    "name": s.name,
                    "fontFamily": s.chars.font_family,
                    "fontStyle": s.chars.font_style,
                    "size": s.chars.size,
                    "align": s.para.align,
                    "direction": s.para.direction,
                    "fill": s.chars.fill,
                })
            })
            .collect();

        let char_styles: Vec<Value> = doc
            .styles
            .character
            .iter()
            .take(64)
            .map(|s| {
                json!({
                    "name": s.name,
                    "fontFamily": s.chars.font_family,
                    "fontStyle": s.chars.font_style,
                    "size": s.chars.size,
                    "fill": s.chars.fill,
                })
            })
            .collect();

        let swatches: Vec<String> = doc.swatches.iter().take(64).map(|s| truncate_utf8(&s.name, 128)).collect();
        let first_margins = doc.spreads.first().and_then(|s| s.pages.first()).map(|p| p.margins);

        Ok(json!({
            "ok": true,
            "title": doc.title,
            "pageCount": doc.page_count(),
            "spreadCount": doc.spreads.len(),
            "facingPages": doc.settings.facing_pages,
            "rightToLeftBinding": doc.settings.right_to_left_binding,
            "pageWidth": doc.settings.page_width,
            "pageHeight": doc.settings.page_height,
            "margins": first_margins,
            "bleed": doc.settings.bleed,
            "sections": doc.sections,
            "textVariables": variables_out,
            "specialMarkers": {
                "currentPageNumber": designcraft_engine::doc::story::PAGE_NUMBER.to_string(),
                "sectionMarker": designcraft_engine::doc::story::SECTION_MARKER.to_string(),
                "nextPageNumber": designcraft_engine::doc::story::NEXT_PAGE_NUMBER.to_string(),
                "previousPageNumber": designcraft_engine::doc::story::PREV_PAGE_NUMBER.to_string(),
            },
            "parentSpreadTargetExample": {
                "kind": "parent",
                "index": 0,
            },
            "parentSpreads": parents_out,
            "paragraphStyles": para_styles,
            "characterStyles": char_styles,
            "swatches": swatches,
            "availableArabicFonts": arabic_fonts,
            "preferredArabicFont": preferred_arabic,
            "userTypographySettings": {
                "arabicTashkeel": request.settings.arabic_tashkeel,
                "arabicFont": request.settings.arabic_font,
                "outputMode": request.settings.output_mode,
            },
            "spreads": spreads_out,
            "preflight": preflight,
        }))
    }

    fn inspect_pages(&self, args: &Value) -> Result<Value, String> {
        let state = self.session.doc().map_err(|e| e.to_string())?;
        let doc = &state.doc;
        let total_pages = doc.page_count();
        if total_pages == 0 {
            return Ok(json!({"ok": true, "pageCount": 0, "pages": []}));
        }
        let raw_start = args.get("start_page").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(1).max(1).min(total_pages);
        let max_end = raw_start.saturating_add(MAX_TOOL_PAGES_PER_CALL.saturating_sub(1)).min(total_pages);
        let raw_end =
            args.get("end_page").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(max_end).max(raw_start).min(total_pages);
        let truncated_range = raw_end > max_end;
        let end_page = raw_end.min(max_end);

        let mut pages_out = Vec::new();
        let mut global_page = 0usize;
        for (spread_idx, spread) in doc.spreads.iter().enumerate() {
            for page in &spread.pages {
                let page_1based = global_page.saturating_add(1);
                if page_1based >= raw_start && page_1based <= end_page {
                    let page_x0 = page.x;
                    let page_x1 = page.x + page.width;
                    let safe = page.margin_rect();
                    let side_str = match page.side {
                        PageSide::Left => "left",
                        PageSide::Right => "right",
                        PageSide::Single => "single",
                    };
                    let outer_edge = outer_edge_side(doc.settings.facing_pages, doc.settings.right_to_left_binding, page.side);
                    let mut items_out = Vec::new();
                    collect_page_items(
                        doc,
                        &self.session.cache,
                        &spread.items,
                        designcraft_geom::Affine::IDENTITY,
                        None,
                        (page_x0, page_x1, spread.pages.len()),
                        0,
                        &mut items_out,
                    );
                    let parent_prefix = page
                        .parent
                        .and_then(|pid| doc.parents.iter().find(|p| p.id == pid))
                        .and_then(|p| p.parent.as_ref())
                        .map(|m| m.prefix.clone());
                    pages_out.push(json!({
                        "page": page_1based,
                        "docPageIndex": global_page,
                        "name": doc.page_name(global_page),
                        "spread": spread_idx,
                        "side": side_str,
                        "outerEdge": outer_edge,
                        "parent": parent_prefix,
                        "pageBoundsInSpread": [page_x0, 0.0, page_x1, page.height],
                        "safeRectInSpread": [safe.x0, safe.y0, safe.x1, safe.y1],
                        "items": items_out,
                    }));
                }
                global_page = global_page.saturating_add(1);
            }
        }

        Ok(json!({
            "ok": true,
            "startPage": raw_start,
            "endPage": end_page,
            "requestedEndPage": raw_end,
            "rangeCappedTo16Pages": truncated_range,
            "totalPages": total_pages,
            "pages": pages_out,
        }))
    }

    fn search_document(&mut self, args: &Value) -> Result<Value, String> {
        let query = args.get("query").and_then(Value::as_str).ok_or("missing string `query`")?;
        if query.is_empty() {
            return Err("`query` must be non-empty".into());
        }
        let grep = args.get("grep").and_then(Value::as_bool).unwrap_or(false);
        let case_sensitive = args.get("case_sensitive").and_then(Value::as_bool).unwrap_or(false);
        let raw = self
            .session
            .execute("find.find", &json!({"find": query, "grep": grep, "caseSensitive": case_sensitive, "scope": "document"}))
            .map_err(|e| e.to_string())?;
        let state = self.session.doc().map_err(|e| e.to_string())?;
        let doc = &state.doc;

        // Map frame ItemId and fallback StoryId -> (1-based page, 0-based spread, optional 0-based parent spread, frame ItemId)
        let mut frame_locations = std::collections::BTreeMap::new();
        let mut story_locations = std::collections::BTreeMap::new();
        let mut story_last_locations = std::collections::BTreeMap::new();
        let mut page_cursor = 0usize;
        for (spread_idx, spread) in doc.spreads.iter().enumerate() {
            let spread_first_page = page_cursor;
            let mut flat = Vec::new();
            flatten_spread_items(&spread.items, designcraft_geom::Affine::IDENTITY, 0, &mut flat);
            for (it, b) in flat {
                if let Some(tf) = it.text_frame() {
                    let mid_x = 0.5 * (b.x0 + b.x1);
                    let rel_page = spread.pages.iter().position(|p| mid_x >= p.x && mid_x <= p.x + p.width).unwrap_or(0);
                    let page_1based = spread_first_page.saturating_add(rel_page).saturating_add(1);
                    let loc = (page_1based, spread_idx, None, it.id.0);
                    frame_locations.insert(it.id.0, loc);
                    story_locations.entry(tf.story.0).or_insert(loc);
                    story_last_locations.insert(tf.story.0, loc);
                }
            }
            page_cursor = page_cursor.saturating_add(spread.pages.len());
        }
        for (parent_idx, parent) in doc.parents.iter().enumerate() {
            let mut flat = Vec::new();
            flatten_spread_items(&parent.items, designcraft_geom::Affine::IDENTITY, 0, &mut flat);
            for (it, _b) in flat {
                if let Some(tf) = it.text_frame() {
                    let loc = (0, 0, Some(parent_idx), it.id.0);
                    frame_locations.insert(it.id.0, loc);
                    story_locations.entry(tf.story.0).or_insert(loc);
                    story_last_locations.entry(tf.story.0).or_insert(loc);
                }
            }
        }

        let arr = raw.as_array().cloned().unwrap_or_default();
        let total_matches = arr.len();
        let mut matches_out = Vec::with_capacity(arr.len().min(128));
        for m in arr.into_iter().take(128) {
            let sid = m.get("story").and_then(Value::as_u64).unwrap_or(0);
            let start = m.get("start").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(0);
            let end = m.get("end").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(start);
            let composed = self.session.cache.get(doc, StoryId(sid), None);
            let frame_hit = composed
                .frames
                .iter()
                .find(|f| start >= f.range.start && (start < f.range.end || (f.range.start == f.range.end && start == f.range.start)))
                .and_then(|f| frame_locations.get(&f.frame.0).copied())
                .or_else(|| {
                    composed
                        .overset_at
                        .filter(|&ov| start >= ov)
                        .and_then(|_| composed.frames.last().and_then(|f| frame_locations.get(&f.frame.0).copied()))
                });
            let (page, spread, parent_spread, frame_id) = frame_hit
                .or_else(|| story_locations.get(&sid).copied())
                .or_else(|| story_last_locations.get(&sid).copied())
                .unwrap_or((0, 0, None, 0));
            let context_snippet = doc.story(StoryId(sid)).map(|st| slice_context(&st.text, start, end, 64)).unwrap_or_default();
            matches_out.push(json!({
                "page": page,
                "spread": spread,
                "parentSpread": parent_spread,
                "frameId": frame_id,
                "story": sid,
                "start": start,
                "end": end,
                "matchedText": m.get("text").cloned().unwrap_or(Value::Null),
                "context": context_snippet,
            }));
        }

        Ok(json!({
            "ok": true,
            "totalMatches": total_matches,
            "truncated": total_matches > matches_out.len(),
            "matches": matches_out,
        }))
    }

    fn inspect_graphics_and_captions(&self, args: &Value) -> Result<Value, String> {
        let state = self.session.doc().map_err(|e| e.to_string())?;
        let doc = &state.doc;
        let total_pages = doc.page_count();
        let start_page =
            args.get("start_page").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).unwrap_or(1).max(1).min(total_pages.max(1));
        let end_page = args
            .get("end_page")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(total_pages)
            .max(start_page)
            .min(total_pages);
        let rtl = doc.settings.right_to_left_binding;

        let mut pages_report = Vec::new();
        let mut composite_count = 0usize;
        let mut page_cursor = 0usize;

        for (spread_idx, spread) in doc.spreads.iter().enumerate() {
            for page in &spread.pages {
                let page_1based = page_cursor.saturating_add(1);
                page_cursor = page_cursor.saturating_add(1);
                if page_1based < start_page || page_1based > end_page {
                    continue;
                }
                let page_x0 = page.x;
                let page_x1 = page.x + page.width;
                let mut flat = Vec::new();
                flatten_spread_items(&spread.items, designcraft_geom::Affine::IDENTITY, 0, &mut flat);

                let on_page = |b: &designcraft_geom::Rect| -> bool {
                    if spread.pages.len() <= 1 {
                        true
                    } else {
                        let mid = 0.5 * (b.x0 + b.x1);
                        mid >= page_x0 - 1.0 && mid <= page_x1 + 1.0
                    }
                };

                let mut graphics = Vec::new();
                let mut texts = Vec::new();
                for (it, b) in &flat {
                    if !on_page(b) {
                        continue;
                    }
                    if matches!(it.content, Content::Graphic(_))
                        || (matches!(it.content, Content::Unassigned) && it.object_style == doc.styles.default_graphic_frame)
                    {
                        graphics.push((*it, *b));
                    } else if let Some(tf) = it.text_frame()
                        && let Some(st) = doc.story(tf.story)
                        && !st.text.trim().is_empty()
                    {
                        texts.push((*it, *b, tf.story.0, st.text.trim().to_string()));
                    }
                }
                if graphics.is_empty() {
                    continue;
                }

                // Sort graphics in deterministic reading order without non-transitive tolerance cycles:
                // 1) Total-order vertical sort by y0, 2) cluster contiguous rows within 24pt, 3) total-order horizontal sort per row.
                sort_graphics_reading_order(&mut graphics, rtl);

                let mut graphic_entries = Vec::new();
                for (g_item, gb) in &graphics {
                    let gw = (gb.x1 - gb.x0).max(0.01);
                    let gh = (gb.y1 - gb.y0).max(0.01);
                    let aspect = gw / gh;
                    let (asset_id, asset_name, pixel_size) = match &g_item.content {
                        Content::Graphic(g) => {
                            if let Some(asset) = doc.assets.get(&g.asset) {
                                (Some(g.asset.0), Some(asset.name.clone()), asset.pixels.map(|(w, h)| [w, h]))
                            } else {
                                (Some(g.asset.0), None, None)
                            }
                        }
                        _ => (None, None, None),
                    };

                    let live_captions: Vec<Value> = texts
                        .iter()
                        .filter(|(t_it, _, _, _)| t_it.live_caption.as_ref().is_some_and(|lc| lc.source == g_item.id))
                        .map(|(t_it, tb, sid, txt)| {
                            json!({"frameId": t_it.id.0, "story": sid, "rect": [tb.x0, tb.y0, tb.x1, tb.y1], "text": truncate_utf8(txt, 256)})
                        })
                        .collect();

                    // Find caption candidates horizontally overlapping this graphic and sitting close below or above it
                    let mut nearby: Vec<(u64, u64, designcraft_geom::Rect, String, String)> = texts
                        .iter()
                        .filter_map(|(t_it, tb, sid, txt)| {
                            let overlap_x = (tb.x1.min(gb.x1) - tb.x0.max(gb.x0)).max(0.0);
                            let tw = (tb.x1 - tb.x0).max(1.0);
                            if overlap_x < tw * 0.35 {
                                return None;
                            }
                            let gap_below = tb.y0 - gb.y1;
                            let gap_above = gb.y0 - tb.y1;
                            if (-18.0..=56.0).contains(&gap_below) || ((0.0..=36.0).contains(&gap_above) && tb.height() <= 40.0) {
                                Some((t_it.id.0, *sid, *tb, txt.clone(), t_it.label.clone()))
                            } else {
                                None
                            }
                        })
                        .collect();

                    // Sort nearby captions in reading order (RTL: rightmost first; LTR: leftmost first)
                    nearby.sort_by(|a, b| {
                        let ax = 0.5 * (a.2.x0 + a.2.x1);
                        let bx = 0.5 * (b.2.x0 + b.2.x1);
                        if rtl { bx.total_cmp(&ax) } else { ax.total_cmp(&bx) }
                    });

                    // Check if multiple captions sit on the same horizontal row under ONE wide composite strip
                    let mut same_row_captions: Vec<&(u64, u64, designcraft_geom::Rect, String, String)> = Vec::new();
                    if let Some(first) = nearby.first() {
                        let ref_y = 0.5 * (first.2.y0 + first.2.y1);
                        for cand in &nearby {
                            let cy = 0.5 * (cand.2.y0 + cand.2.y1);
                            if (cy - ref_y).abs() <= 20.0 && cand.2.width() < gw * 0.6 {
                                same_row_captions.push(cand);
                            }
                        }
                    }

                    let is_composite_strip = aspect >= 1.9 && same_row_captions.len() >= 2;
                    let suggested_cards: Vec<Value> = if is_composite_strip {
                        composite_count = composite_count.saturating_add(1);
                        same_row_captions
                            .iter()
                            .enumerate()
                            .map(|(idx, (fid, sid, cb, txt, _))| {
                                let cx0 = cb.x0.max(gb.x0);
                                let cx1 = cb.x1.min(gb.x1);
                                let card_rect = if cx1 - cx0 >= 32.0 { [cx0, gb.y0, cx1, gb.y1] } else { [cb.x0, gb.y0, cb.x1, gb.y1] };
                                json!({
                                    "readingOrder": idx.saturating_add(1),
                                    "captionFrameId": fid,
                                    "captionStory": sid,
                                    "captionText": truncate_utf8(txt, 160),
                                    "suggestedGraphicRect": card_rect,
                                })
                            })
                            .collect()
                    } else {
                        Vec::new()
                    };

                    let nearby_json: Vec<Value> = nearby
                        .iter()
                        .enumerate()
                        .map(|(idx, (fid, sid, cb, txt, lbl))| {
                            json!({
                                "readingOrder": idx.saturating_add(1),
                                "frameId": fid,
                                "story": sid,
                                "label": lbl,
                                "rect": [cb.x0, cb.y0, cb.x1, cb.y1],
                                "text": truncate_utf8(txt, 256),
                            })
                        })
                        .collect();

                    graphic_entries.push(json!({
                        "frameId": g_item.id.0,
                        "label": g_item.label,
                        "altText": g_item.alt_text,
                        "rect": [gb.x0, gb.y0, gb.x1, gb.y1],
                        "aspectRatio": (aspect * 100.0).round() / 100.0,
                        "assetId": asset_id,
                        "assetName": asset_name,
                        "pixelSize": pixel_size,
                        "linkedLiveCaptions": live_captions,
                        "nearbyCaptionsInReadingOrder": nearby_json,
                        "compositeStripCandidate": is_composite_strip,
                        "suggestedIndividualCards": suggested_cards,
                    }));
                }

                pages_report.push(json!({
                    "page": page_1based,
                    "spread": spread_idx,
                    "readingDirection": if rtl { "rightToLeft (rightmost X = 1st item)" } else { "leftToRight (leftmost X = 1st item)" },
                    "graphics": graphic_entries,
                }));
            }
        }

        Ok(json!({
            "ok": true,
            "startPage": start_page,
            "endPage": end_page,
            "rightToLeftBinding": rtl,
            "compositeStripCandidatesFound": composite_count,
            "pages": pages_report,
        }))
    }

    fn apply_engine_commands(&mut self, args: &Value) -> Result<Value, String> {
        if self.applied_chunks.len() >= MAX_CHUNKS {
            return Err(format!("Exceeded maximum of {MAX_CHUNKS} command batches per task; call complete_task."));
        }
        let raw_commands = args.get("commands").ok_or("missing `commands` array")?;
        let mut actions: Vec<EngineAction> = serde_path_to_error::deserialize(raw_commands).map_err(|e| format!("commands: {e}"))?;
        if actions.is_empty() || actions.len() > MAX_ACTIONS {
            return Err(format!("commands must contain 1 to {MAX_ACTIONS} actions per call"));
        }
        let mut temp_plan = designcraft_ai_types::protocol::ProductionPlan { title: "Agent chunk".into(), commands: actions, images: Vec::new() };
        crate::native_layout::normalize_command_aliases(&mut temp_plan);
        crate::worker::validate_actions(&temp_plan.commands, &self.catalog, OutputMode::NativeOnly)?;
        actions = temp_plan.commands;

        let apply_res = self
            .session
            .execute(
                "production.apply",
                &json!({
                    "commands": actions,
                    "outputMode": "native_only",
                    "label": "Agent step",
                    "requirePreflight": false,
                    "minPpi": self.min_ppi,
                }),
            )
            .map_err(|e| e.to_string())?;

        let chunk_index = self.applied_chunks.len();
        let steps_applied = actions.len();
        self.applied_chunks.push(actions);
        let preflight = self.preflight_summary()?;
        Ok(json!({
            "ok": true,
            "chunkIndex": chunk_index,
            "stepsApplied": steps_applied,
            "totalAppliedActions": self.total_applied_actions(),
            "results": apply_res.get("results").cloned().unwrap_or(Value::Null),
            "preflight": preflight,
        }))
    }

    fn queue_image_generations(&mut self, args: &Value) -> Result<Value, String> {
        if !self.allow_image_queue {
            return Err(
                "No valid image generation model is configured in AI settings; configure Gemini, OpenAI, or ComfyUI image settings first.".into()
            );
        }
        if self.deferred_images.len() >= MAX_IMAGE_BATCHES {
            return Err(format!(
                "Exceeded maximum of {MAX_IMAGE_BATCHES} queued image batches ({}) per task",
                MAX_IMAGE_BATCHES.saturating_mul(MAX_IMAGES)
            ));
        }
        let raw_images = args.get("images").and_then(Value::as_array).ok_or("missing `images` array")?;
        if raw_images.is_empty() || raw_images.len() > MAX_IMAGES {
            return Err(format!("`images` must contain 1 to {MAX_IMAGES} placements per call (invoke again for more pages)"));
        }
        let summary = args
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Regenerate damaged or mismatched illustrations")
            .to_string();

        let companion_commands: Vec<EngineAction> = match args.get("companion_commands") {
            Some(v) if !v.is_null() => {
                let mut cmds: Vec<EngineAction> = serde_path_to_error::deserialize(v).map_err(|e| format!("companion_commands: {e}"))?;
                if cmds.len() > MAX_ACTIONS {
                    return Err(format!("companion_commands exceeds {MAX_ACTIONS} actions"));
                }
                if !cmds.is_empty() {
                    let mut temp_plan =
                        designcraft_ai_types::protocol::ProductionPlan { title: "Companion layout".into(), commands: cmds, images: Vec::new() };
                    crate::native_layout::normalize_command_aliases(&mut temp_plan);
                    crate::worker::validate_actions(&temp_plan.commands, &self.catalog, OutputMode::NativeOnly)?;
                    cmds = temp_plan.commands;
                }
                cmds
            }
            _ => Vec::new(),
        };

        // Simulate prior queued companion_commands + this batch's companion_commands on a clone of the current draft session
        let mut sim = Session::new();
        sim.prefs = self.session.prefs.clone();
        let draft_doc = self.session.doc().map_err(|e| e.to_string())?.clone();
        sim.add_document(draft_doc);
        for prior in &self.deferred_images {
            if !prior.companion_commands.is_empty() {
                sim.execute(
                    "production.apply",
                    &json!({
                        "commands": prior.companion_commands,
                        "outputMode": "native_only",
                        "label": "Prior companion commands",
                        "requirePreflight": false,
                        "minPpi": self.min_ppi,
                    }),
                )
                .map_err(|e| format!("prior companion_commands conflict with current draft: {e}"))?;
            }
        }
        if !companion_commands.is_empty() {
            sim.execute(
                "production.apply",
                &json!({
                    "commands": companion_commands,
                    "outputMode": "native_only",
                    "label": "Simulate companion commands",
                    "requirePreflight": false,
                    "minPpi": self.min_ppi,
                }),
            )
            .map_err(|e| format!("companion_commands validation failed on current draft: {e}"))?;
        }

        let sim_doc = &sim.doc().map_err(|e| e.to_string())?.doc;
        let spread_count = sim_doc.spreads.len();
        let mut placements = Vec::with_capacity(raw_images.len());
        for (idx, item) in raw_images.iter().enumerate() {
            let spread = item
                .get("spread")
                .or_else(|| item.get("page"))
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| format!("images[{idx}]: missing 0-based `spread` index"))?;
            if spread >= spread_count {
                return Err(format!("images[{idx}]: spread {spread} is out of range (0..{spread_count})"));
            }
            let rect_arr = item.get("rect").and_then(Value::as_array).ok_or_else(|| format!("images[{idx}]: missing `rect` [x0,y0,x1,y1]"))?;
            if rect_arr.len() != 4 {
                return Err(format!("images[{idx}]: `rect` must have 4 numbers [x0,y0,x1,y1]"));
            }
            let mut rect = [0.0f64; 4];
            for (ri, rv) in rect_arr.iter().enumerate() {
                let val = rv.as_f64().filter(|v| v.is_finite()).ok_or_else(|| format!("images[{idx}].rect[{ri}]: invalid number"))?;
                if let Some(slot) = rect.get_mut(ri) {
                    *slot = val;
                }
            }
            if rect[2] - rect[0] < 12.0 || rect[3] - rect[1] < 12.0 {
                return Err(format!("images[{idx}]: rect must be at least 12x12 points"));
            }
            let prompt = item
                .get("prompt")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty() && s.len() <= 8000)
                .ok_or_else(|| format!("images[{idx}]: `prompt` must be 1..8000 UTF-8 bytes"))?
                .to_string();
            let label = item
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| truncate_utf8(s, 240))
                .unwrap_or_else(|| format!("Illustration {}", idx.saturating_add(1)));

            placements.push(ImagePlacement {
                family_id: None,
                slot_id: None,
                repeat_rects: Vec::new(),
                repeat_groups: Vec::new(),
                prompt,
                page: spread,
                rect,
                label,
            });
        }
        crate::design::check_document_image_slots(sim.doc().map_err(|e| e.to_string())?, &placements)?;

        // Check total actions headroom: companion_commands + 3 * placements <= MAX_ACTIONS
        if companion_commands.len().saturating_add(placements.len().saturating_mul(3)) > MAX_ACTIONS {
            return Err("companion_commands plus image placement commands exceed the 512-action single-plan limit".into());
        }

        let count = placements.len();
        self.deferred_images.push(DeferredImagePlan { companion_commands, images: placements, summary: truncate_utf8(&summary, 512) });

        Ok(json!({
            "ok": true,
            "queuedImagesInBatch": count,
            "totalQueuedImages": self.total_queued_images(),
            "note": "Queued illustrations validated cleanly against draft spread geometry.",
        }))
    }

    fn complete_task(&mut self, args: &Value, remaining_turns: u32) -> Result<Value, String> {
        let title = args
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| truncate_utf8(s, 200))
            .unwrap_or_else(|| "AI Document Edit".into());
        let summary = args
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| truncate_utf8(s, 1024))
            .unwrap_or_else(|| title.clone());

        if self.applied_chunks.is_empty() && self.deferred_images.is_empty() {
            return Err("No native commands or image placements have been applied yet; call `apply_engine_commands` or `queue_image_generations` before `complete_task`.".into());
        }

        let preflight = self.preflight_summary()?;
        if self.has_blocking_preflight(&preflight) && remaining_turns > 0 {
            let errors = preflight.get("errors").and_then(Value::as_u64).unwrap_or(0);
            let overset = preflight.get("oversetErrors").and_then(Value::as_u64).unwrap_or(0);
            return Err(format!(
                "Preflight reports {errors} error(s) ({overset} overset frame(s)) on the edited document (`production.apply` requires 0 preflight errors). Resolve them first (e.g. call `apply_engine_commands` with `{{\"command\":\"font.replace\",\"params\":{{\"script\":\"arabic\",\"toFamily\":\"auto\",\"redefineStyles\":true}}}}` for missing Arabic glyphs or `{{\"command\":\"story.fitOverset\",\"params\":{{\"minSize\":8.0}}}}` for overset text) before calling `complete_task`. Preflight: {preflight}"
            ));
        }

        self.completed_title = Some(title);
        self.completed_summary = Some(summary);
        Ok(json!({
            "ok": true,
            "completed": true,
            "appliedChunks": self.applied_chunks.len(),
            "totalActions": self.total_applied_actions(),
            "queuedImages": self.total_queued_images(),
            "preflight": preflight,
        }))
    }
    fn render_visual_check(&mut self, args: &Value) -> Result<(Value, Vec<ImageInput>), String> {
        let state = self.session.doc().map_err(|e| e.to_string())?.clone();
        let total_pages: usize = state.doc.spreads.iter().map(|sp| sp.pages.len()).sum();
        if total_pages == 0 {
            return Err("Document has no pages to render".into());
        }
        let preflight = self.preflight_summary()?;
        if let Some(raw_pages) = args.get("pages").filter(|v| !v.is_null()) {
            let page_arr = raw_pages.as_array().ok_or("`pages` must be an array of 1..=4 1-based page numbers")?;
            if page_arr.is_empty() || page_arr.len() > MAX_VISUAL_IMAGES_PER_TURN {
                return Err(format!("`pages` must contain 1 to {MAX_VISUAL_IMAGES_PER_TURN} page numbers per call"));
            }
            let mut page_nums = Vec::with_capacity(page_arr.len());
            for (idx, val) in page_arr.iter().enumerate() {
                let p = val
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| format!("`pages[{idx}]` must be a 1-based integer page number"))?;
                page_nums.push(p);
            }
            let mut pages_out = Vec::with_capacity(page_nums.len());
            let mut images = Vec::new();
            for p in page_nums {
                let (page_payload, maybe_img) = self.render_single_visual_page(&state, total_pages, p, args)?;
                if let Some(img) = maybe_img {
                    images.push(img);
                }
                pages_out.push(page_payload);
            }
            return Ok((
                json!({
                    "ok": true,
                    "pages": pages_out,
                    "visionAttached": !images.is_empty(),
                    "visionAttachedCount": images.len(),
                    "preflight": preflight,
                }),
                images,
            ));
        }

        let page_1based =
            args.get("page").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok()).ok_or("missing 1-based `page` (or `pages`) number")?;
        let (mut payload, maybe_img) = self.render_single_visual_page(&state, total_pages, page_1based, args)?;
        let mut images = Vec::new();
        if let Some(img) = maybe_img {
            images.push(img);
        }
        if let Some(obj) = payload.as_object_mut() {
            obj.insert("ok".into(), json!(true));
            obj.insert("preflight".into(), preflight);
        }
        Ok((payload, images))
    }

    fn render_single_visual_page(
        &mut self,
        state: &DocState,
        total_pages: usize,
        page_1based: usize,
        args: &Value,
    ) -> Result<(Value, Option<ImageInput>), String> {
        if page_1based == 0 || page_1based > total_pages {
            return Err(format!("`page` {page_1based} is out of range (1..={total_pages})"));
        }
        let abs_page = page_1based - 1;
        let (spread_idx, page_in_spread) = state.doc.page_loc(abs_page).ok_or("unable to locate page in document spreads")?;
        let spread = state.doc.spreads.get(spread_idx).ok_or("spread index out of bounds")?;
        let page_obj = spread.pages.get(page_in_spread).ok_or("page index out of bounds")?;
        let page_bounds = page_obj.bounds();

        let coord_space = args.get("coord_space").and_then(Value::as_str).unwrap_or("spread");
        if !matches!(coord_space, "spread" | "page") {
            return Err(format!("`coord_space` must be \"spread\" or \"page\" (got `{coord_space}`)"));
        }

        let is_crop = args.get("rect").is_some_and(|v| !v.is_null());
        let target_rect = if let Some(raw_rect) = args.get("rect").filter(|v| !v.is_null()) {
            let arr = raw_rect.as_array().ok_or("`rect` must be an array of 4 numbers [x0, y0, x1, y1]")?;
            if arr.len() != 4 {
                return Err("`rect` must contain exactly 4 numbers [x0, y0, x1, y1]".into());
            }
            let mut coords = [0.0f64; 4];
            for (idx, v) in arr.iter().enumerate() {
                let n = v.as_f64().filter(|x| x.is_finite()).ok_or_else(|| format!("`rect[{idx}]` must be a finite number"))?;
                if let Some(slot) = coords.get_mut(idx) {
                    *slot = n;
                }
            }
            let (dx, dy) = if coord_space == "page" { (page_bounds.x0, page_bounds.y0) } else { (0.0, 0.0) };
            let r = designcraft_geom::Rect::new(coords[0] + dx, coords[1] + dy, coords[2] + dx, coords[3] + dy);
            let w_pt = r.x1 - r.x0;
            let h_pt = r.y1 - r.y0;
            if w_pt < 8.0 || h_pt < 8.0 {
                return Err("`rect` width and height must each be at least 8 points".into());
            }
            if w_pt > 5000.0 || h_pt > 5000.0 {
                return Err("`rect` width and height must not exceed 5000 points".into());
            }
            let allowed_area = page_bounds.inflate(144.0, 144.0);
            let overlap_w = r.x1.min(allowed_area.x1) - r.x0.max(allowed_area.x0);
            let overlap_h = r.y1.min(allowed_area.y1) - r.y0.max(allowed_area.y0);
            if overlap_w <= 0.0 || overlap_h <= 0.0 {
                return Err(format!(
                    "`rect` [{}, {}, {}, {}] does not intersect page {page_1based} bounds [{}, {}, {}, {}] in spread coordinates (pass `coord_space: \"page\"` for page-local coordinates, or pass the matching `page` number)",
                    round2(r.x0),
                    round2(r.y0),
                    round2(r.x1),
                    round2(r.y1),
                    round2(page_bounds.x0),
                    round2(page_bounds.y0),
                    round2(page_bounds.x1),
                    round2(page_bounds.y1)
                ));
            }
            r
        } else {
            page_bounds
        };

        let max_side = args.get("max_side").and_then(Value::as_u64).map(|n| n.clamp(256, 1440) as u32).unwrap_or(960);
        let highlight_issues = args.get("highlight_issues").and_then(Value::as_bool).unwrap_or(false);

        let max_pt = target_rect.width().max(target_rect.height()).max(1.0);
        let scale = (f64::from(max_side) / max_pt).clamp(0.05, 4.0);
        let px_w = (target_rect.width() * scale).round().clamp(1.0, 1440.0) as u32;
        let px_h = (target_rect.height() * scale).round().clamp(1.0, 1440.0) as u32;
        let view = designcraft_geom::Affine::scale(scale) * designcraft_geom::Affine::translate(-target_rect.origin().to_vec2());
        let opts = RenderOptions {
            background: Some([255, 255, 255, 255]),
            paper: true,
            highlight_missing_fonts: highlight_issues,
            highlight_hj: highlight_issues,
            ..RenderOptions::default()
        };

        let rendered = self.renderer.render(
            &state.doc,
            &self.session.cache,
            &[Placed { spread: SpreadRef::Doc(spread_idx), xf: designcraft_geom::Affine::IDENTITY }],
            px_w,
            px_h,
            view,
            &opts,
        );

        let straight = rendered.to_straight();
        let total_px = (u64::from(rendered.width) * u64::from(rendered.height)).max(1);
        let mut non_white_px = 0u64;
        let mut min_x = rendered.width;
        let mut min_y = rendered.height;
        let mut max_x = 0u32;
        let mut max_y = 0u32;
        for (idx, rgba) in straight.chunks_exact(4).enumerate() {
            if rgba[0] < 250 || rgba[1] < 250 || rgba[2] < 250 {
                non_white_px = non_white_px.saturating_add(1);
                let x = (idx as u32) % rendered.width.max(1);
                let y = (idx as u32) / rendered.width.max(1);
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        let ink_bbox_px = if non_white_px > 0 { json!([min_x, min_y, max_x.saturating_add(1), max_y.saturating_add(1)]) } else { Value::Null };
        let ink_ratio = round2((non_white_px as f64 / total_px as f64) * 100.0) / 100.0;

        let mut attached_img = None;
        let mut vision_note: Option<&'static str> = None;
        if self.supports_vision {
            if self.visual_images_in_turn < MAX_VISUAL_IMAGES_PER_TURN {
                let remaining_bytes = self.max_visual_bytes_per_turn.saturating_sub(self.visual_bytes_in_turn);
                let png_cap = (420 * 1024).min(remaining_bytes);
                let jpg_cap = (480 * 1024).min(remaining_bytes);
                let png = rendered.to_png();
                let mut encoded_img =
                    if !png.is_empty() && png.len() <= png_cap { Some(ImageInput { mime_type: "image/png".into(), bytes: png }) } else { None };
                if encoded_img.is_none() && jpg_cap > 0 {
                    for quality in [82u8, 64, 46] {
                        let jpg = rendered.to_jpeg(quality);
                        if !jpg.is_empty() && jpg.len() <= jpg_cap {
                            encoded_img = Some(ImageInput { mime_type: "image/jpeg".into(), bytes: jpg });
                            break;
                        }
                    }
                }
                if encoded_img.is_none() && remaining_bytes >= 16 * 1024 && (px_w > 320 || px_h > 320) {
                    let half_scale = (scale * 0.5).clamp(0.05, 4.0);
                    let half_w = (target_rect.width() * half_scale).round().clamp(1.0, 720.0) as u32;
                    let half_h = (target_rect.height() * half_scale).round().clamp(1.0, 720.0) as u32;
                    let half_view =
                        designcraft_geom::Affine::scale(half_scale) * designcraft_geom::Affine::translate(-target_rect.origin().to_vec2());
                    let downscaled = self.renderer.render(
                        &state.doc,
                        &self.session.cache,
                        &[Placed { spread: SpreadRef::Doc(spread_idx), xf: designcraft_geom::Affine::IDENTITY }],
                        half_w,
                        half_h,
                        half_view,
                        &opts,
                    );
                    let jpg = downscaled.to_jpeg(60);
                    if !jpg.is_empty() && jpg.len() <= remaining_bytes {
                        encoded_img = Some(ImageInput { mime_type: "image/jpeg".into(), bytes: jpg });
                    }
                }
                if let Some(img) = encoded_img {
                    self.visual_images_in_turn = self.visual_images_in_turn.saturating_add(1);
                    self.visual_bytes_in_turn = self.visual_bytes_in_turn.saturating_add(img.bytes.len());
                    attached_img = Some(img);
                } else {
                    vision_note = Some(
                        "Per-turn 2 MiB inline visual budget reached; invoke `render_visual_check` in a subsequent turn or crop a smaller `rect`",
                    );
                }
            } else {
                vision_note = Some(
                    "Maximum of 4 visual render attachments per turn reached; invoke `render_visual_check` in a subsequent turn for additional visual frames",
                );
            }
        } else {
            vision_note = Some("Selected text model is text-only; returning rendered pixel ink metrics and preflight summary");
        }

        let mut payload = json!({
            "page": page_1based,
            "spread": spread_idx,
            "mode": if is_crop { "crop" } else { "page" },
            "renderBoundsInSpread": [round2(target_rect.x0), round2(target_rect.y0), round2(target_rect.x1), round2(target_rect.y1)],
            "pixelSize": [rendered.width, rendered.height],
            "scalePxPerPt": round2(scale),
            "inkCoverageRatio": ink_ratio,
            "inkBoundingBoxPx": ink_bbox_px,
            "visionAttached": attached_img.is_some(),
        });
        if let Some(note) = vision_note
            && let Some(obj) = payload.as_object_mut()
        {
            obj.insert("visionNote".into(), json!(note));
        }
        Ok((payload, attached_img))
    }

    pub(crate) fn has_blocking_preflight(&self, preflight: &Value) -> bool {
        preflight.get("errors").and_then(Value::as_u64).unwrap_or(0) > 0
    }

    pub(crate) fn preflight_summary(&mut self) -> Result<Value, String> {
        let report = self.session.execute("preflight.run", &json!({"minPpi": self.min_ppi})).map_err(|e| e.to_string())?;
        let errors = report.get("errors").and_then(Value::as_u64).unwrap_or(0);
        let warnings = report.get("warnings").and_then(Value::as_u64).unwrap_or(0);
        let all_issues = report.get("issues").and_then(Value::as_array);
        let overset_errors =
            all_issues.map(|arr| arr.iter().filter(|it| it.get("kind").and_then(Value::as_str) == Some("overset")).count() as u64).unwrap_or(0);
        let issues: Vec<Value> = all_issues.map(|arr| arr.iter().take(32).cloned().collect()).unwrap_or_default();
        Ok(json!({
            "errors": errors,
            "oversetErrors": overset_errors,
            "warnings": warnings,
            "issues": issues,
        }))
    }
}

pub(crate) fn tool_definitions(allow_image_queue: bool) -> Vec<ToolDefinition> {
    let mut tools = vec![
        ToolDefinition {
            name: "inspect_document_overview".into(),
            description: "Inspect full document structure: page count, facing-page & RTL binding geometry, spreads, parent spreads, sections, text variables (with UTF-8 marker chars), styles, swatches, installed Arabic fonts, and preflight status.".into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "inspect_pages".into(),
            description: "Inspect all native items, stories, fonts, diacritics, tables, live captions, and placed graphic metadata on pages `start_page`..=`end_page` (1-based inclusive, up to 16 pages per call; invoke multiple calls in parallel to inspect larger books).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "start_page": {"type": "integer", "description": "1-based start page number (>= 1)."},
                    "end_page": {"type": "integer", "description": "1-based inclusive end page number (at most start_page + 15 per call)."}
                },
                "required": ["start_page", "end_page"]
            }),
        },
        ToolDefinition {
            name: "search_document".into(),
            description: "Search across all stories in the document (plain text or regular expression via `grep: true`), returning 1-based page numbers, 0-based spread indices, frame IDs, story IDs, UTF-8 byte offsets, and surrounding context.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Text or regex pattern to find."},
                    "grep": {"type": "boolean", "description": "True to evaluate `query` as a regular expression."},
                    "case_sensitive": {"type": "boolean", "description": "True for case-sensitive matching."}
                },
                "required": ["query"]
            }),
        },
        ToolDefinition {
            name: "inspect_graphics_and_captions".into(),
            description: "Analyze all graphic frames and their linked or nearby captions in reading order (right-to-left on RTL documents, left-to-right on LTR documents), flagging wide multi-subject composite strips and suggesting individual 1:1 card rectangles aligned with each caption.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "start_page": {"type": "integer", "description": "Optional 1-based start page (defaults to 1)."},
                    "end_page": {"type": "integer", "description": "Optional 1-based end page (defaults to last page)."}
                }
            }),
        },
        ToolDefinition {
            name: "render_visual_check".into(),
            description: "Render a live visual proof of a document page (or a cropped `[x0, y0, x1, y1]` region) via DesignCraft's CPU renderer so you can visually inspect actual typography, Arabic shaping/diacritics, RTL reading alignment, running headers/folios, image crops, spacing, and visual hierarchy before completing a task.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "page": {"type": "integer", "description": "1-based page number to render (>= 1). Provide either `page` or `pages`."},
                    "pages": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "description": "Optional array of 1..=4 1-based page numbers to render in one call."
                    },
                    "rect": {
                        "type": "array",
                        "items": {"type": "number"},
                        "description": "Optional [x0, y0, x1, y1] crop rectangle in points (defaults to full page bounds)."
                    },
                    "coord_space": {
                        "type": "string",
                        "enum": ["spread", "page"],
                        "description": "Coordinate space for `rect`: `spread` (default, matching `spreadBounds` and `safeRectInSpread`) or `page` (matching `pageLocalBounds`)."
                    },
                    "max_side": {
                        "type": "integer",
                        "description": "Optional target maximum pixel dimension (256..=1440, default 960)."
                    },
                    "highlight_issues": {
                        "type": "boolean",
                        "description": "Optional: highlight missing font glyphs and H&J composition violations in the rendered proof."
                    }
                }
            }),
        },
        ToolDefinition {
            name: "apply_engine_commands".into(),
            description: "Validate and atomically execute a batch of 1..512 native DesignCraft engine commands on the private draft document. Supports `$N.field` references within the batch and returns real created IDs plus live preflight/overset feedback.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "commands": {
                        "type": "array",
                        "description": "Ordered array of `{command, params}` actions from the vetted native production catalog.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "command": {"type": "string"},
                                "params": {"type": "object"}
                            },
                            "required": ["command", "params"]
                        }
                    },
                    "summary": {"type": "string", "description": "Brief description of what this command batch changes."}
                },
                "required": ["commands"]
            }),
        },
    ];
    if allow_image_queue {
        tools.push(ToolDefinition {
            name: "queue_image_generations".into(),
            description: "Queue clean single-subject raster illustrations for new or damaged/composite/mismatched graphic frames. Do NOT delete existing composite frames inside `apply_engine_commands` if replacing them requires new images; instead put destructive frame-replacement commands (`edit.clear` on old composite frame, `frame.create` for individual 1:1 graphic cards, `object.altText`, `object.caption` with `live: true`, `object.group`) inside `companion_commands` here so they execute atomically ONLY when the illustrations are generated.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "summary": {"type": "string", "description": "Human-readable explanation of which pages/cards need regenerated illustrations and why."},
                    "companion_commands": {
                        "type": "array",
                        "description": "Optional native commands executed atomically right before placing the generated images (e.g. deleting an old composite strip frame and creating individual 1:1 graphic frames + live captions).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "command": {"type": "string"},
                                "params": {"type": "object"}
                            },
                            "required": ["command", "params"]
                        }
                    },
                    "images": {
                        "type": "array",
                        "description": "Individual single-subject illustration placements (one subject per frame, NO text/letters/numbers in image).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "spread": {"type": "integer", "description": "0-based spread index where the graphic frame lives."},
                                "rect": {
                                    "type": "array",
                                    "items": {"type": "number"},
                                    "description": "[left, top, right, bottom] in spread points matching an existing or `companion_commands`-created graphic frame."
                                },
                                "prompt": {"type": "string", "description": "Detailed visual prompt for ONE isolated subject matching its bound caption, with zero text, letters, or numbers."},
                                "label": {"type": "string", "description": "Short label matching the bound caption/subject."}
                            },
                            "required": ["spread", "rect", "prompt", "label"]
                        }
                    }
                },
                "required": ["summary", "images"]
            }),
        });
    }
    tools.push(ToolDefinition {
        name: "complete_task".into(),
        description: "Verify zero preflight/overset errors and finalize the document editing task for atomic single-Undo publication.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "Concise undo-history label summarizing the completed edits."},
                "summary": {"type": "string", "description": "Concise summary of all verified edits applied across the document."}
            },
            "required": ["title", "summary"]
        }),
    });
    tools
}

fn outer_edge_side(facing_pages: bool, rtl_binding: bool, side: PageSide) -> &'static str {
    if !facing_pages {
        return if rtl_binding { "left" } else { "right" };
    }
    match side {
        PageSide::Left => "left",
        PageSide::Right => "right",
        PageSide::Single => {
            if rtl_binding {
                "left"
            } else {
                "right"
            }
        }
    }
}

fn item_kind_name(item: &Item) -> &'static str {
    match &item.content {
        Content::Text(_) => "text",
        Content::Graphic(_) => "graphic",
        Content::Group { .. } => "group",
        Content::Unassigned => match item.shape {
            Shape::GraphicLine => "line",
            Shape::Path => "path",
            _ => "unassigned",
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_page_items(
    doc: &designcraft_engine::doc::Document,
    cache: &designcraft_engine::compose::Cache,
    items: &[std::sync::Arc<Item>],
    parent_xf: designcraft_geom::Affine,
    parent_group: Option<ItemId>,
    page_span: (f64, f64, usize),
    depth: usize,
    out: &mut Vec<Value>,
) {
    if depth > 16 || out.len() >= 512 {
        return;
    }
    let (page_x0, page_x1, pages_in_spread) = page_span;
    for item_arc in items {
        let item = item_arc.as_ref();
        if out.len() >= 512 {
            break;
        }
        let world_xf = parent_xf * item.xf;
        let b = world_xf.transform_rect_bbox(item.inner_bounds());
        if pages_in_spread > 1 {
            let overlap = (b.x1.min(page_x1) - b.x0.max(page_x0)).max(0.0);
            let mid = 0.5 * (b.x0 + b.x1);
            if overlap <= 0.5 && !(mid >= page_x0 && mid <= page_x1) {
                continue;
            }
        }
        let live_caption = item.live_caption.as_ref().map(|lc| json!({"source": lc.source.0, "template": lc.template}));
        let mut entry = json!({
            "id": item.id.0,
            "parentGroup": parent_group.map(|g| g.0),
            "kind": item_kind_name(item),
            "locked": item.locked,
            "label": item.label,
            "altText": item.alt_text,
            "liveCaption": live_caption,
            "spreadBounds": [round2(b.x0), round2(b.y0), round2(b.x1), round2(b.y1)],
            "pageLocalBounds": [round2(b.x0 - page_x0), round2(b.y0), round2(b.x1 - page_x0), round2(b.y1)],
            "fillSwatch": item.fill.swatch,
            "fillTint": item.fill.tint,
            "strokeSwatch": item.stroke.swatch,
            "strokeWeight": round2(item.stroke.weight),
        });

        match &item.content {
            Content::Text(tf) => {
                if let Some(st) = doc.story(tf.story) {
                    let first_para = st.paras.first().cloned().unwrap_or_default();
                    let (para_props, base_char) = doc.styles.resolve_para(&first_para);
                    let char_props = doc.styles.resolve_char(&base_char, st.char_format_at(0));
                    let has_diacritics = st.text.chars().any(is_arabic_tashkeel_mark);
                    let overset = cache.get(doc, tf.story, None).overset_at.is_some();
                    let tables: Vec<Value> = st
                        .tables
                        .values()
                        .take(16)
                        .map(|tbl| {
                            let cols = tbl.columns.len();
                            let cells: Vec<Value> = tbl
                                .cells
                                .iter()
                                .take(64)
                                .enumerate()
                                .map(|(ci, cell)| {
                                    let r = ci.checked_div(cols).unwrap_or(0);
                                    let c = ci.checked_rem(cols).unwrap_or(0);
                                    json!({"row": r, "col": c, "text": truncate_utf8(&cell.text.text, 256)})
                                })
                                .collect();
                            json!({"table": tbl.id, "rows": tbl.rows.len(), "cols": cols, "cells": cells})
                        })
                        .collect();
                    if let Some(obj) = entry.as_object_mut() {
                        obj.insert("story".into(), json!(tf.story.0));
                        obj.insert("text".into(), json!(truncate_utf8(&st.text, MAX_STORY_TOOL_BYTES)));
                        obj.insert("hasDiacritics".into(), json!(has_diacritics));
                        obj.insert("direction".into(), json!(para_props.direction));
                        obj.insert("align".into(), json!(para_props.align));
                        obj.insert("paraStyle".into(), json!(first_para.style));
                        obj.insert("fontFamily".into(), json!(char_props.font_family));
                        obj.insert("fontStyle".into(), json!(char_props.font_style));
                        obj.insert("fontSize".into(), json!(round2(char_props.size)));
                        obj.insert("textFill".into(), json!(char_props.fill));
                        obj.insert("overset".into(), json!(overset));
                        if !tables.is_empty() {
                            obj.insert("tables".into(), json!(tables));
                        }
                    }
                }
            }
            Content::Graphic(g) => {
                if let Some(obj) = entry.as_object_mut() {
                    let w = (b.x1 - b.x0).max(0.01);
                    let h = (b.y1 - b.y0).max(0.01);
                    obj.insert("aspectRatio".into(), json!(round2(w / h)));
                    obj.insert("assetId".into(), json!(g.asset.0));
                    if let Some(asset) = doc.assets.get(&g.asset) {
                        obj.insert("assetName".into(), json!(truncate_utf8(&asset.name, 160)));
                        if let Some((px_w, px_h)) = asset.pixels {
                            obj.insert("pixelSize".into(), json!([px_w, px_h]));
                            let ppi_x = (f64::from(px_w) / (w / 72.0)).round();
                            let ppi_y = (f64::from(px_h) / (h / 72.0)).round();
                            obj.insert("effectivePpi".into(), json!(ppi_x.min(ppi_y)));
                        }
                    }
                }
            }
            Content::Group { items: children } => {
                out.push(entry);
                collect_page_items(doc, cache, children, world_xf, Some(item.id), page_span, depth.saturating_add(1), out);
                continue;
            }
            Content::Unassigned => {}
        }
        out.push(entry);
    }
}

fn flatten_spread_items<'a>(
    items: &'a [std::sync::Arc<Item>],
    parent_xf: designcraft_geom::Affine,
    depth: usize,
    out: &mut Vec<(&'a Item, designcraft_geom::Rect)>,
) {
    if depth > 16 || out.len() >= 2048 {
        return;
    }
    for item_arc in items {
        let item = item_arc.as_ref();
        let world_xf = parent_xf * item.xf;
        let b = world_xf.transform_rect_bbox(item.inner_bounds());
        if let Content::Group { items: children } = &item.content {
            flatten_spread_items(children, world_xf, depth.saturating_add(1), out);
        } else {
            out.push((item, b));
        }
    }
}

fn is_arabic_tashkeel_mark(c: char) -> bool {
    matches!(c, '\u{064B}'..='\u{065F}' | '\u{0670}')
}

fn round2(v: f64) -> f64 {
    if v.is_finite() { (v * 100.0).round() / 100.0 } else { 0.0 }
}

fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.get(..end).unwrap_or("").to_string()
}

fn sort_graphics_reading_order(graphics: &mut [(&Item, designcraft_geom::Rect)], rtl: bool) {
    graphics.sort_by(|(ia, a), (ib, b)| {
        a.y0.total_cmp(&b.y0).then_with(|| (0.5 * (a.x0 + a.x1)).total_cmp(&(0.5 * (b.x0 + b.x1)))).then_with(|| ia.id.0.cmp(&ib.id.0))
    });
    let mut row_start = 0usize;
    while row_start < graphics.len() {
        let anchor_y = graphics[row_start].1.y0;
        let mut row_end = row_start.saturating_add(1);
        while row_end < graphics.len() && (graphics[row_end].1.y0 - anchor_y) <= 24.0 {
            row_end = row_end.saturating_add(1);
        }
        if let Some(row) = graphics.get_mut(row_start..row_end) {
            row.sort_by(|(ia, a), (ib, b)| {
                let ax = 0.5 * (a.x0 + a.x1);
                let bx = 0.5 * (b.x0 + b.x1);
                let horiz = if rtl { bx.total_cmp(&ax) } else { ax.total_cmp(&bx) };
                horiz.then_with(|| a.y0.total_cmp(&b.y0)).then_with(|| ia.id.0.cmp(&ib.id.0))
            });
        }
        row_start = row_end;
    }
}

fn slice_context(s: &str, start: usize, end: usize, radius: usize) -> String {
    let len = s.len();
    let mut c0 = start.min(len).saturating_sub(radius);
    while c0 > 0 && !s.is_char_boundary(c0) {
        c0 -= 1;
    }
    let mut c1 = end.min(len).saturating_add(radius).min(len);
    while c1 < len && !s.is_char_boundary(c1) {
        c1 += 1;
    }
    s.get(c0..c1).unwrap_or("").to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use designcraft_ai_types::protocol::{AiSettings, TaskTarget};

    #[test]
    fn render_visual_check_renders_full_page_and_crop_and_respects_vision_caps() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"pages": 2, "facingPages": true, "width": 612, "height": 792})).unwrap();
        s.execute("frame.create", &json!({"spread": 0, "rect": [72, 72, 300, 180], "content": "unassigned"})).unwrap();
        s.execute("object.fill", &json!({"swatch": "[Black]"})).unwrap();
        let base = s.doc().unwrap().clone();
        let req = TaskRequest {
            prompt: "Visually inspect page 1".into(),
            settings: AiSettings::default(),
            skill_ids: Vec::new(),
            auto_select_skills: false,
            target: TaskTarget::CurrentDocument,
            export_directory: None,
        };

        let mut ws = DocToolWorkspace::new(&base, &s.prefs, designcraft_engine::cmd::production::catalog(), 150.0);
        // Full-page render attaches valid PNG/JPEG image + non-zero ink metrics:
        let (res, imgs, is_err) = ws.execute_tool_multimodal("render_visual_check", &json!({"page": 1, "max_side": 512}), &req, 5);
        assert!(!is_err, "{res}");
        assert_eq!(res["ok"], true);
        assert_eq!(res["mode"], "page");
        assert_eq!(res["visionAttached"], true);
        assert!(res["inkCoverageRatio"].as_f64().unwrap() > 0.01);
        assert!(res["inkBoundingBoxPx"].is_array());
        assert_eq!(imgs.len(), 1);
        assert!(imgs[0].bytes.starts_with(b"\x89PNG") || imgs[0].bytes.starts_with(&[0xFF, 0xD8]));

        // Cropped region render in page-local coordinates:
        let (crop_res, crop_imgs, is_err) = ws.execute_tool_multimodal(
            "render_visual_check",
            &json!({"page": 1, "rect": [60, 60, 320, 200], "coord_space": "page", "max_side": 400}),
            &req,
            5,
        );
        assert!(!is_err, "{crop_res}");
        assert_eq!(crop_res["mode"], "crop");
        assert_eq!(crop_imgs.len(), 1);
        assert!(crop_res["inkCoverageRatio"].as_f64().unwrap() > 0.5);

        // Per-turn 4-image cap degrades 5th call in same turn to metadata without error:
        for _ in 0..2 {
            let (_, more_imgs, err) = ws.execute_tool_multimodal("render_visual_check", &json!({"page": 1, "max_side": 320}), &req, 5);
            assert!(!err);
            assert_eq!(more_imgs.len(), 1);
        }
        let (fifth_res, fifth_imgs, err) = ws.execute_tool_multimodal("render_visual_check", &json!({"page": 1, "max_side": 320}), &req, 5);
        assert!(!err);
        assert!(fifth_imgs.is_empty());
        assert_eq!(fifth_res["visionAttached"], false);

        // Text-only mode omits image bytes while still computing real rendered ink metrics:
        ws.supports_vision = false;
        ws.visual_images_in_turn = 0;
        let (txt_res, txt_imgs, err) = ws.execute_tool_multimodal("render_visual_check", &json!({"page": 1, "max_side": 320}), &req, 5);
        assert!(!err);
        assert!(txt_imgs.is_empty());
        assert_eq!(txt_res["visionAttached"], false);
        assert!(txt_res["inkCoverageRatio"].as_f64().unwrap() > 0.01);

        // Hostile / invalid inputs return structured tool errors without panicking:
        for bad_args in [
            json!({"page": 0}),
            json!({"page": 99}),
            json!({"page": 1, "rect": [10, 10, 12, 12]}),
            json!({"page": 1, "rect": [20000, 20000, 20200, 20200]}),
            json!({"page": 1, "coord_space": "invalid"}),
        ] {
            let (_err_res, err_imgs, is_err) = ws.execute_tool_multimodal("render_visual_check", &bad_args, &req, 5);
            assert!(is_err, "expected error for {bad_args}");
            assert!(err_imgs.is_empty());
        }

        // Multi-page facing spread: passing page-local coords without coord_space:"page" for right-hand page 3
        // (which sits at x=612..1224 on spread 1) must be rejected instead of silently cropping page 2 (x=0..612):
        let mut s_facing = Session::new();
        s_facing.execute("file.new", &json!({"pages": 3, "facingPages": true, "width": 612, "height": 792})).unwrap();
        let facing_base = s_facing.doc().unwrap().clone();
        let mut ws_facing = DocToolWorkspace::new(&facing_base, &s_facing.prefs, designcraft_engine::cmd::production::catalog(), 150.0);
        let (wrong_page_res, wrong_imgs, wrong_err) =
            ws_facing.execute_tool_multimodal("render_visual_check", &json!({"page": 3, "rect": [48, 48, 300, 300]}), &req, 5);
        assert!(wrong_err, "expected page-bounds mismatch error, got {wrong_page_res}");
        assert!(wrong_imgs.is_empty());
        assert!(wrong_page_res["error"].as_str().unwrap_or_default().contains("coord_space: \"page\""));

        let (ok_page_local, ok_imgs, ok_err) =
            ws_facing.execute_tool_multimodal("render_visual_check", &json!({"page": 3, "rect": [48, 48, 300, 300], "coord_space": "page"}), &req, 5);
        assert!(!ok_err, "{ok_page_local}");
        assert_eq!(ok_imgs.len(), 1);
        assert_eq!(ok_page_local["renderBoundsInSpread"][0], json!(660.0));

        // Tight per-turn byte budget accurately synchronizes visionAttachedCount with retained images:
        ws_facing.visual_images_in_turn = 0;
        ws_facing.visual_bytes_in_turn = 0;
        ws_facing.max_visual_bytes_per_turn = ok_imgs[0].bytes.len() + 64;
        let (multi_res, multi_imgs, multi_err) =
            ws_facing.execute_tool_multimodal("render_visual_check", &json!({"pages": [1, 2, 3], "max_side": 512}), &req, 5);
        assert!(!multi_err, "{multi_res}");
        assert_eq!(
            multi_res["visionAttachedCount"].as_u64().unwrap() as usize,
            multi_imgs.len(),
            "visionAttachedCount must exactly match delivered image count under byte budget"
        );
    }

    #[test]
    fn inspect_graphics_staggered_coordinates_never_panic_and_threaded_search_resolves_frame_pages() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"pages": 3, "facingPages": false, "width": 612, "height": 2400})).unwrap();
        // 100 staggered graphics whose 20pt vertical steps previously formed a non-transitive comparator cycle `(200-2i, 20i)` that panicked `sort_by`.
        for i in 0..100usize {
            let x0 = 500.0 - (i as f64) * 4.0;
            let y0 = 20.0 + (i as f64) * 20.0;
            s.execute("frame.create", &json!({"spread": 0, "rect": [x0, y0, x0 + 24.0, y0 + 18.0], "content": "graphic"})).unwrap();
        }
        // Threaded story spanning Page 1 (spread 0) -> Page 3 (spread 2):
        let f1 = s
            .execute(
                "frame.create",
                &json!({"spread": 0, "rect": [40, 2100, 200, 2132], "content": "text", "text": "Opening line on page one.\nOverflowing paragraph pushed to page three target_marker_omega."}),
            )
            .unwrap();
        let f3 = s.execute("frame.create", &json!({"spread": 2, "rect": [40, 40, 360, 220], "content": "text", "text": ""})).unwrap();
        s.execute("text.thread", &json!({"from": f1["id"], "to": f3["id"]})).unwrap();

        let base = s.doc().unwrap().clone();
        let mut ws = DocToolWorkspace::new(&base, &s.prefs, designcraft_engine::cmd::production::catalog(), 150.0);
        let req = TaskRequest {
            prompt: "audit".into(),
            settings: AiSettings::default(),
            skill_ids: Vec::new(),
            auto_select_skills: false,
            target: TaskTarget::CurrentDocument,
            export_directory: None,
        };

        let (gfx_res, _, gfx_err) = ws.execute_tool_multimodal("inspect_graphics_and_captions", &json!({"start_page": 1, "end_page": 1}), &req, 5);
        assert!(!gfx_err, "{gfx_res}");
        assert_eq!(gfx_res["pages"][0]["graphics"].as_array().unwrap().len(), 100);

        let (search_res, _, search_err) = ws.execute_tool_multimodal("search_document", &json!({"query": "target_marker_omega"}), &req, 5);
        assert!(!search_err, "{search_res}");
        let hit = &search_res["matches"][0];
        assert_eq!(hit["page"], 3, "threaded match displayed in frame on page 3 must report page 3, got {hit}");
        assert_eq!(hit["spread"], 2);
        assert_eq!(hit["frameId"], f3["id"]);
    }
}
