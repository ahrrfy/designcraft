//! 3-Layer Bilingual (Arabic + English) Pure-Rust BM25 Local RAG & Reusable Knowledge Packs.
//!
//! Operates 100% in CPU memory with zero GPU VRAM allocation and zero Ollama model-swap churn:
//! - **Layer 1:** Per-task `attachments` (`KnowledgeReference` / `Auto`) + persisted Knowledge Packs
//!   under `<ai_root>/knowledge-packs/<pack_id>/{manifest.json,chunks.json}`.
//! - **Layer 2:** Active / approved [`SkillSnapshot`] instructions and references.
//! - **Layer 3:** Native Engine Command Catalog (`70+` commands from `designcraft_engine::command_specs()`
//!   enriched with `production::catalog()` parameter schemas and bilingual aliases).

use designcraft_ai_providers::ImageInput;
use designcraft_ai_skills::SkillSnapshot;
use designcraft_ai_types::protocol::{AttachmentRole, KnowledgePackSummary, TaskAttachment};
use designcraft_engine::file_access::{self, Access, AutomationRoots};
use image::{DynamicImage, RgbImage, codecs::jpeg::JpegEncoder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_PACK_FILES: usize = 32;
const MAX_PACK_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CHUNKS_PER_PACK: usize = 1024;
const MAX_CHUNKS_IN_INDEX: usize = 4096;
const MAX_CHUNK_CHARS: usize = 1200;
const MAX_SCANNED_VISION_PAGES: usize = 4;
const MAX_SCANNED_SIDE: u32 = 1280;
const MAX_SCANNED_JPEG_BYTES: usize = 200 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeLayer {
    #[default]
    ProjectOrPack,
    ApprovedSkill,
    EngineCommand,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeChunk {
    pub chunk_id: String,
    pub layer: KnowledgeLayer,
    pub source_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_or_sheet: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section_heading: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RankedKnowledgeHit {
    pub score: f64,
    pub chunk: KnowledgeChunk,
}

pub struct IngestedAttachmentBundle {
    pub chunks: Vec<KnowledgeChunk>,
    pub scanned_page_images: Vec<ImageInput>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedPackManifest {
    pub schema_version: u32,
    pub summary: KnowledgePackSummary,
}

/// Normalize Arabic & English text for bilingual BM25 tokenization, stripping tashkeel/tatweel,
/// normalizing Arabic Presentation Forms (`FB50..FDFF`, `FE70..FEFF`), unifying Alef/`ى`/`ة`,
/// and inserting spaces across Arabic/Latin boundaries.
pub fn normalize_bilingual_text(input: &str) -> String {
    let canonical = designcraft_pdf::extract::normalize_arabic_presentation_forms(input);
    let mut out = String::with_capacity(canonical.len().saturating_add(16));
    let mut prev_script: u8 = 0; // 1 = Arabic, 2 = Latin/Digit
    for ch in canonical.chars() {
        if matches!(ch, '\u{0610}'..='\u{061a}' | '\u{0640}' | '\u{064b}'..='\u{065f}' | '\u{0670}' | '\u{06d6}'..='\u{06ed}') {
            continue;
        }
        let mapped = match ch {
            '\u{0660}'..='\u{0669}' => char::from_u32(u32::from('0') + (u32::from(ch) - u32::from('\u{0660}'))).unwrap_or('0'),
            '\u{06F0}'..='\u{06F9}' => char::from_u32(u32::from('0') + (u32::from(ch) - u32::from('\u{06F0}'))).unwrap_or('0'),
            'أ' | 'إ' | 'آ' | 'ٱ' => 'ا',
            'ة' => 'ه',
            'ى' => 'ي',
            _ => ch,
        };
        let cur_script = if ('\u{0600}'..='\u{06ff}').contains(&mapped) {
            1
        } else if mapped.is_ascii_alphanumeric() {
            2
        } else {
            0
        };
        if prev_script != 0 && cur_script != 0 && prev_script != cur_script && !out.ends_with(' ') {
            out.push(' ');
        }
        for lc in mapped.to_lowercase() {
            out.push(lc);
        }
        prev_script = cur_script;
    }
    out
}

fn stem_arabic_token(tok: &str) -> Option<String> {
    let mut current = tok;
    for prefix in ["وال", "بال", "كال", "فال", "لل", "ال"] {
        if let Some(rest) = current.strip_prefix(prefix)
            && rest.chars().count() >= 3
        {
            current = rest;
            break;
        }
    }
    for single_pref in ["و", "ب", "ف", "ل"] {
        if let Some(rest) = current.strip_prefix(single_pref)
            && rest.chars().count() >= 4
        {
            if let Some(after_al) = rest.strip_prefix("ال")
                && after_al.chars().count() >= 3
            {
                current = after_al;
            } else {
                current = rest;
            }
            break;
        }
    }
    for suffix in ["ات", "ون", "ين", "ان", "هم", "ها", "كم", "يه", "ه"] {
        if let Some(rest) = current.strip_suffix(suffix)
            && rest.chars().count() >= 3
        {
            current = rest;
            break;
        }
    }
    (current != tok && current.chars().count() >= 2).then(|| current.to_string())
}

fn stem_english_token(tok: &str) -> Option<String> {
    if !tok.is_ascii() || tok.len() < 4 {
        return None;
    }
    if let Some(rest) = tok.strip_suffix("ies")
        && rest.len() >= 2
    {
        return Some(format!("{rest}y"));
    }
    for suffix in ["ation", "ments", "ment", "ness", "ings", "ing", "able", "ible", "ed", "es", "s"] {
        if let Some(rest) = tok.strip_suffix(suffix)
            && rest.len() >= 3
        {
            return Some(rest.to_string());
        }
    }
    None
}

/// Extract weighted query/document features: exact normalized token (`weight = 14`) + light stem (`weight = 10`).
fn extract_weighted_terms(text: &str) -> Vec<(String, u32)> {
    let norm = normalize_bilingual_text(text);
    let mut terms = Vec::new();
    for raw in norm.split(|c: char| !c.is_alphanumeric() && c != '.' && c != '_' && c != '-') {
        let tok = raw.trim_matches(|c: char| c == '.' || c == '_' || c == '-');
        if tok.is_empty() || tok.chars().count() < 2 {
            continue;
        }
        terms.push((format!("e:{tok}"), 14));
        // Also split dotted/hyphenated identifiers (`toc.generate`, `layout.createAlternate`).
        if tok.contains('.') || tok.contains('_') || tok.contains('-') {
            for part in tok.split(['.', '_', '-']) {
                if part.chars().count() >= 2 {
                    terms.push((format!("e:{part}"), 12));
                    if let Some(stem) = stem_english_token(part) {
                        terms.push((format!("s:{stem}"), 9));
                    }
                }
            }
        }
        let has_arabic = tok.chars().any(|c| ('\u{0600}'..='\u{06ff}').contains(&c));
        if has_arabic {
            if let Some(stem) = stem_arabic_token(tok) {
                terms.push((format!("s:{stem}"), 10));
            }
        } else if let Some(stem) = stem_english_token(tok) {
            terms.push((format!("s:{stem}"), 10));
        }
    }
    terms
}

#[derive(Clone, Debug, Default)]
struct IndexedDoc {
    chunk: KnowledgeChunk,
    term_weights: BTreeMap<String, u32>,
    doc_len: u32,
}

/// In-memory Pure-Rust Bilingual BM25 Retriever across Layers 1 & 2 (or Layer 3).
#[derive(Clone, Debug, Default)]
pub struct KnowledgeIndex {
    docs: Vec<IndexedDoc>,
    doc_freq: BTreeMap<String, usize>,
    total_doc_len: u64,
}

impl KnowledgeIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    pub fn chunks(&self) -> impl Iterator<Item = &KnowledgeChunk> {
        self.docs.iter().map(|d| &d.chunk)
    }

    pub fn push_chunk(&mut self, chunk: KnowledgeChunk) {
        if self.docs.len() >= MAX_CHUNKS_IN_INDEX || chunk.text.trim().is_empty() {
            return;
        }
        let mut combined = String::with_capacity(chunk.text.len().saturating_add(128));
        combined.push_str(&chunk.source_name);
        combined.push(' ');
        if let Some(ps) = &chunk.page_or_sheet {
            combined.push_str(ps);
            combined.push(' ');
        }
        if let Some(h) = &chunk.section_heading {
            combined.push_str(h);
            combined.push(' ');
        }
        combined.push_str(&chunk.text);

        let weighted = extract_weighted_terms(&combined);
        let mut term_weights: BTreeMap<String, u32> = BTreeMap::new();
        let mut doc_len = 0u32;
        for (term, w) in weighted {
            doc_len = doc_len.saturating_add(w);
            let entry = term_weights.entry(term).or_insert(0);
            *entry = entry.saturating_add(w);
        }
        if doc_len == 0 {
            return;
        }
        for term in term_weights.keys() {
            let df = self.doc_freq.entry(term.clone()).or_insert(0);
            *df = df.saturating_add(1);
        }
        self.total_doc_len = self.total_doc_len.saturating_add(u64::from(doc_len));
        self.docs.push(IndexedDoc { chunk, term_weights, doc_len });
    }

    pub fn extend_chunks(&mut self, chunks: impl IntoIterator<Item = KnowledgeChunk>) {
        for chunk in chunks {
            if self.docs.len() >= MAX_CHUNKS_IN_INDEX {
                break;
            }
            self.push_chunk(chunk);
        }
    }

    /// Index active/approved skills as Layer 2 (`ApprovedSkill`) chunks.
    pub fn index_approved_skills(&mut self, skills: &[SkillSnapshot]) {
        for skill in skills {
            let heading = skill.design_profile.as_ref().map(|p| p.name.clone()).unwrap_or_else(|| skill.skill_id.clone());
            for (idx, part) in split_text_into_chunks(&skill.instructions, MAX_CHUNK_CHARS).into_iter().enumerate() {
                self.push_chunk(KnowledgeChunk {
                    chunk_id: format!("skill:{}:{idx}", skill.skill_id),
                    layer: KnowledgeLayer::ApprovedSkill,
                    source_name: format!("skill:{}", skill.skill_id),
                    page_or_sheet: None,
                    section_heading: Some(heading.clone()),
                    text: part,
                });
            }
            for (ref_path, ref_body) in &skill.references {
                for (idx, part) in split_text_into_chunks(ref_body, MAX_CHUNK_CHARS).into_iter().enumerate() {
                    self.push_chunk(KnowledgeChunk {
                        chunk_id: format!("skill:{}:{ref_path}:{idx}", skill.skill_id),
                        layer: KnowledgeLayer::ApprovedSkill,
                        source_name: format!("skill:{}/{ref_path}", skill.skill_id),
                        page_or_sheet: None,
                        section_heading: Some(heading.clone()),
                        text: part,
                    });
                }
            }
        }
    }

    /// Run BM25 search (`k1 = 1.5`, `b = 0.75`) over the indexed chunks.
    pub fn search(&self, query: &str, top_k: usize) -> Vec<RankedKnowledgeHit> {
        self.search_filtered(query, top_k, None)
    }

    /// Run BM25 search (`k1 = 1.5`, `b = 0.75`) with an optional [`KnowledgeLayer`] filter.
    pub fn search_filtered(&self, query: &str, top_k: usize, layer_filter: Option<KnowledgeLayer>) -> Vec<RankedKnowledgeHit> {
        let limit = top_k.clamp(1, 32);
        if self.docs.is_empty() || query.trim().is_empty() {
            return Vec::new();
        }
        let query_terms = extract_weighted_terms(query);
        if query_terms.is_empty() {
            return Vec::new();
        }
        let mut dedup_q: BTreeMap<String, u32> = BTreeMap::new();
        for (term, q_w) in query_terms {
            let entry = dedup_q.entry(term).or_insert(0);
            *entry = (*entry).max(q_w);
        }

        let n_docs = self.docs.len() as f64;
        let avgdl = (self.total_doc_len as f64 / n_docs).max(1.0);
        let k1 = 1.5_f64;
        let b = 0.75_f64;

        let mut scored: Vec<RankedKnowledgeHit> = Vec::new();
        for doc in &self.docs {
            if let Some(expected_layer) = layer_filter
                && doc.chunk.layer != expected_layer
            {
                continue;
            }
            let mut score = 0.0_f64;
            for (q_term, q_weight) in &dedup_q {
                let Some(&tf_raw) = doc.term_weights.get(q_term) else {
                    continue;
                };
                let df = self.doc_freq.get(q_term).copied().unwrap_or(1) as f64;
                let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
                let tf = f64::from(tf_raw);
                let dl = f64::from(doc.doc_len);
                let denom = tf + k1 * (1.0 - b + b * (dl / avgdl));
                if denom > 0.0 {
                    let term_boost = f64::from(*q_weight) / 10.0;
                    score += idf * ((tf * (k1 + 1.0)) / denom) * term_boost;
                }
            }
            if doc.chunk.layer == KnowledgeLayer::ProjectOrPack {
                score *= 1.1; // Slight priority for explicit user project docs/packs over generic skill prose
            }
            if score > 0.0 && score.is_finite() {
                scored.push(RankedKnowledgeHit { score, chunk: doc.chunk.clone() });
            }
        }
        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        scored
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

/// Split document text into paragraph/heading-aware chunks bounded by `max_chars`.
pub fn split_text_into_chunks(text: &str, max_chars: usize) -> Vec<String> {
    let cap = max_chars.max(128);
    let mut chunks = Vec::new();
    let mut current = String::new();
    for para in text.split("\n\n") {
        let trimmed = para.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !current.is_empty() && current.chars().count().saturating_add(trimmed.chars().count()).saturating_add(2) > cap {
            chunks.push(std::mem::take(&mut current));
        }
        if trimmed.chars().count() > cap {
            let mut slice_buf = String::new();
            for ch in trimmed.chars() {
                slice_buf.push(ch);
                if slice_buf.chars().count() >= cap {
                    chunks.push(std::mem::take(&mut slice_buf));
                }
            }
            if !slice_buf.trim().is_empty() {
                current = slice_buf;
            }
        } else {
            if !current.is_empty() {
                current.push_str("\n\n");
            }
            current.push_str(trimmed);
        }
        if chunks.len() >= MAX_CHUNKS_PER_PACK {
            break;
        }
    }
    if !current.trim().is_empty() && chunks.len() < MAX_CHUNKS_PER_PACK {
        chunks.push(current);
    }
    chunks
}

fn read_local_regular_file(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    file_access::check_read(path)?;
    if !path.is_absolute() {
        return Err(format!("Attachment path must be an absolute local path: {}", path.display()));
    }
    let bytes = designcraft_engine::io::read_capped(path, limit).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    if bytes.len() as u64 > limit {
        return Err(format!("Attachment {} exceeds size limit ({limit} bytes)", path.display()));
    }
    Ok(bytes)
}

fn render_scanned_pdf_pages(bytes: &[u8], scanned_1based: &[usize], max_pages: usize) -> Vec<ImageInput> {
    if scanned_1based.is_empty() || max_pages == 0 {
        return Vec::new();
    }
    let Ok(pdf) = hayro::hayro_syntax::Pdf::new(std::sync::Arc::new(bytes.to_vec())) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for &page_num in scanned_1based.iter().take(max_pages) {
        let idx = page_num.saturating_sub(1);
        let Some(page) = pdf.pages().get(idx) else {
            continue;
        };
        let (w, h) = page.render_dimensions();
        if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
            continue;
        }
        let scale = (MAX_SCANNED_SIDE as f32 / w.max(h)).min(4.0);
        let width = (w * scale).floor().clamp(1.0, MAX_SCANNED_SIDE as f32) as u16;
        let height = (h * scale).floor().clamp(1.0, MAX_SCANNED_SIDE as f32) as u16;
        let settings = hayro::RenderSettings { x_scale: scale, y_scale: scale, width: Some(width), height: Some(height), ..Default::default() };
        let pixmap = hayro::render(page, &hayro::RenderCache::new(), &hayro::hayro_interpret::InterpreterSettings::default(), &settings);
        let pixels: Vec<u8> = pixmap
            .data_as_u8_slice()
            .chunks_exact(4)
            .flat_map(|c| {
                let [r, g, b, a] = c else {
                    return [255, 255, 255];
                };
                [r.saturating_add(255 - a), g.saturating_add(255 - a), b.saturating_add(255 - a)]
            })
            .collect();
        let Some(raster) = RgbImage::from_raw(u32::from(width), u32::from(height), pixels) else {
            continue;
        };
        let mut current = DynamicImage::ImageRgb8(raster);
        for _ in 0..5 {
            let mut jpeg = Vec::new();
            if JpegEncoder::new_with_quality(&mut jpeg, 70).encode_image(&current).is_ok() && jpeg.len() <= MAX_SCANNED_JPEG_BYTES {
                out.push(ImageInput { mime_type: "image/jpeg".into(), bytes: jpeg });
                break;
            }
            let side = current.width().max(current.height()).saturating_mul(3) / 4;
            current = current.thumbnail(side.max(1), side.max(1));
        }
    }
    out
}

/// Ingest raw file bytes of `.pdf`, `.xlsx`, `.docx`, `.rtf`, `.csv`, `.tsv`, `.md`, or `.txt`
/// into structured [`KnowledgeChunk`]s with page/worksheet citations + optional scanned-page vision fallbacks.
pub fn ingest_document_bytes(
    source_name: &str,
    bytes: &[u8],
    allow_scanned_raster: bool,
) -> Result<(Vec<KnowledgeChunk>, Vec<ImageInput>, Vec<String>), String> {
    let ext = Path::new(source_name).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let mut chunks = Vec::new();
    let mut scanned_images = Vec::new();
    let mut warnings = Vec::new();

    match ext.as_str() {
        "pdf" => {
            let report = designcraft_pdf::extract_text_pages(bytes)?;
            let mut scanned_pages = Vec::new();
            for page in &report.pages {
                if page.is_likely_scanned || page.text.chars().filter(|c| !c.is_whitespace()).count() < 16 {
                    scanned_pages.push(page.page_number);
                }
                let trimmed = page.text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                for (sub_idx, piece) in split_text_into_chunks(trimmed, MAX_CHUNK_CHARS).into_iter().enumerate() {
                    if chunks.len() >= MAX_CHUNKS_PER_PACK {
                        break;
                    }
                    chunks.push(KnowledgeChunk {
                        chunk_id: format!("{source_name}:p{}:{sub_idx}", page.page_number),
                        layer: KnowledgeLayer::ProjectOrPack,
                        source_name: source_name.to_string(),
                        page_or_sheet: Some(format!("page {}", page.page_number)),
                        section_heading: first_heading_line(&piece),
                        text: piece,
                    });
                }
            }
            if !scanned_pages.is_empty() {
                warnings.push(format!("{source_name}: {} likely scanned page(s) detected", scanned_pages.len()));
                if allow_scanned_raster {
                    scanned_images = render_scanned_pdf_pages(bytes, &scanned_pages, MAX_SCANNED_VISION_PAGES);
                }
            }
        }
        "xlsx" | "docx" | "rtf" | "csv" | "tsv" | "md" | "markdown" | "txt" => {
            let extracted = designcraft_textimport::extract_structured_text(source_name, bytes)
                .map_err(|e| format!("Cannot extract text from {source_name}: {e}"))?;
            warnings.extend(extracted.warnings);

            // 1. Index structured tables / worksheets with sheet/table citations.
            for (t_idx, table) in extracted.tables.iter().enumerate() {
                let sheet_label = if table.name.trim().is_empty() { format!("Table {}", t_idx + 1) } else { table.name.clone() };
                let header_line = table.headers.join(" | ");
                let mut row_batch = String::new();
                let mut batch_num = 0usize;
                for (r_idx, row) in table.rows.iter().enumerate() {
                    let formatted_row = if table.headers.is_empty() {
                        row.join(" | ")
                    } else {
                        table
                            .headers
                            .iter()
                            .zip(row.iter())
                            .filter(|(_, val)| !val.trim().is_empty())
                            .map(|(h, v)| format!("{h}: {v}"))
                            .collect::<Vec<_>>()
                            .join(" | ")
                    };
                    if formatted_row.trim().is_empty() {
                        continue;
                    }
                    if row_batch.is_empty() && !header_line.trim().is_empty() {
                        row_batch.push_str(&format!("Columns: {header_line}\n"));
                    }
                    row_batch.push_str(&format!("Row {}: {formatted_row}\n", r_idx + 1));
                    if row_batch.chars().count() >= MAX_CHUNK_CHARS {
                        if chunks.len() < MAX_CHUNKS_PER_PACK {
                            chunks.push(KnowledgeChunk {
                                chunk_id: format!("{source_name}:t{t_idx}:{batch_num}"),
                                layer: KnowledgeLayer::ProjectOrPack,
                                source_name: source_name.to_string(),
                                page_or_sheet: Some(sheet_label.clone()),
                                section_heading: Some(sheet_label.clone()),
                                text: std::mem::take(&mut row_batch),
                            });
                            batch_num = batch_num.saturating_add(1);
                        } else {
                            break;
                        }
                    }
                }
                if !row_batch.trim().is_empty() && chunks.len() < MAX_CHUNKS_PER_PACK {
                    chunks.push(KnowledgeChunk {
                        chunk_id: format!("{source_name}:t{t_idx}:{batch_num}"),
                        layer: KnowledgeLayer::ProjectOrPack,
                        source_name: source_name.to_string(),
                        page_or_sheet: Some(sheet_label.clone()),
                        section_heading: Some(sheet_label),
                        text: row_batch,
                    });
                }
            }

            // 2. For non-spreadsheet prose documents (.docx, .rtf, .md, .txt), also index prose & notes.
            if !matches!(ext.as_str(), "xlsx" | "csv" | "tsv") {
                let prose = if !extracted.markdown.trim().is_empty() { &extracted.markdown } else { &extracted.plain_text };
                for (p_idx, part) in split_text_into_chunks(prose, MAX_CHUNK_CHARS).into_iter().enumerate() {
                    if chunks.len() >= MAX_CHUNKS_PER_PACK {
                        break;
                    }
                    chunks.push(KnowledgeChunk {
                        chunk_id: format!("{source_name}:prose:{p_idx}"),
                        layer: KnowledgeLayer::ProjectOrPack,
                        source_name: source_name.to_string(),
                        page_or_sheet: None,
                        section_heading: first_heading_line(&part),
                        text: part,
                    });
                }
                if !extracted.notes.is_empty() && chunks.len() < MAX_CHUNKS_PER_PACK {
                    let joined_notes = extracted.notes.join("\n");
                    for (n_idx, part) in split_text_into_chunks(&joined_notes, MAX_CHUNK_CHARS).into_iter().enumerate() {
                        if chunks.len() >= MAX_CHUNKS_PER_PACK {
                            break;
                        }
                        chunks.push(KnowledgeChunk {
                            chunk_id: format!("{source_name}:notes:{n_idx}"),
                            layer: KnowledgeLayer::ProjectOrPack,
                            source_name: source_name.to_string(),
                            page_or_sheet: Some("footnotes".into()),
                            section_heading: Some("Footnotes & Endnotes".into()),
                            text: part,
                        });
                    }
                }
            }
        }
        _ => return Err(format!("Unsupported knowledge document format: {source_name}")),
    }

    Ok((chunks, scanned_images, warnings))
}

fn first_heading_line(chunk: &str) -> Option<String> {
    let first = chunk.lines().find(|l| !l.trim().is_empty())?.trim();
    let stripped = first.trim_start_matches('#').trim();
    if !stripped.is_empty() && stripped.chars().count() <= 96 { Some(stripped.to_string()) } else { None }
}

pub fn is_knowledge_doc_extension(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    matches!(ext.as_str(), "pdf" | "xlsx" | "docx" | "rtf" | "csv" | "tsv" | "md" | "markdown" | "txt")
}

/// Ingest per-task [`TaskAttachment`] files with `KnowledgeReference`, `DataRecords`, or `Auto` document roles.
pub fn ingest_task_attachments(attachments: &[TaskAttachment]) -> Result<IngestedAttachmentBundle, String> {
    let mut chunks = Vec::new();
    let mut scanned_page_images = Vec::new();
    let mut warnings = Vec::new();

    for att in attachments.iter().take(MAX_PACK_FILES) {
        let clean_path = designcraft_ai_types::sanitize_user_path(&att.path);
        if clean_path.is_empty() {
            continue;
        }
        let path = Path::new(&clean_path);
        match att.role {
            AttachmentRole::BrandLogo | AttachmentRole::ContentAsset => continue,
            AttachmentRole::Auto if !is_knowledge_doc_extension(path) => continue,
            AttachmentRole::KnowledgeReference | AttachmentRole::DataRecords | AttachmentRole::Auto => {}
        }
        let bytes = read_local_regular_file(path, MAX_PACK_FILE_BYTES)?;
        let source_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| clean_path.clone());
        let allow_vision = scanned_page_images.len() < MAX_SCANNED_VISION_PAGES;
        let (file_chunks, mut file_imgs, file_warnings) = ingest_document_bytes(&source_name, &bytes, allow_vision)?;
        let room = MAX_SCANNED_VISION_PAGES.saturating_sub(scanned_page_images.len());
        file_imgs.truncate(room);
        scanned_page_images.extend(file_imgs);
        warnings.extend(file_warnings);
        for chunk in file_chunks {
            if chunks.len() >= MAX_CHUNKS_IN_INDEX {
                break;
            }
            chunks.push(chunk);
        }
    }

    Ok(IngestedAttachmentBundle { chunks, scanned_page_images, warnings })
}

/// Extract pre-ingested scanned attachment page images from `context` (removing the transient
/// `_scanned_attachment_images` field so base64 bytes do not bloat prompt JSON), falling back to
/// bounded on-demand attachment ingestion when `context` was constructed without host pre-enrichment.
pub fn resolve_scanned_attachment_images(context: &mut Value, request: &designcraft_ai_types::protocol::TaskRequest) -> Vec<ImageInput> {
    use base64::Engine as _;
    let taken = context.as_object_mut().and_then(|obj| obj.remove("_scanned_attachment_images"));
    if !designcraft_ai_providers::supports_agent_vision(&request.settings.text_model) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut total_bytes = 0usize;
    if let Some(arr) = taken.as_ref().and_then(Value::as_array) {
        for item in arr.iter().take(MAX_SCANNED_VISION_PAGES) {
            let Some(media_type) = item.get("mediaType").and_then(Value::as_str) else {
                continue;
            };
            let Some(b64) = item.get("base64").and_then(Value::as_str) else {
                continue;
            };
            let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) else {
                continue;
            };
            if bytes.is_empty() || total_bytes.saturating_add(bytes.len()) > 1024 * 1024 {
                continue;
            }
            total_bytes = total_bytes.saturating_add(bytes.len());
            out.push(ImageInput { mime_type: media_type.to_string(), bytes });
        }
        if !out.is_empty() {
            return out;
        }
    }
    if taken.is_none()
        && !request.attachments.is_empty()
        && let Ok(bundle) = ingest_task_attachments(&request.attachments)
    {
        for img in bundle.scanned_page_images.into_iter().take(MAX_SCANNED_VISION_PAGES) {
            if total_bytes.saturating_add(img.bytes.len()) > 1024 * 1024 {
                continue;
            }
            total_bytes = total_bytes.saturating_add(img.bytes.len());
            out.push(img);
        }
    }
    out
}

// ============================================================================
// Persisted Knowledge Packs (<ai_root>/knowledge-packs/<pack_id>/{manifest.json,chunks.json})
// ============================================================================

pub fn slugify_pack_id(name: &str) -> String {
    let mut slug = String::new();
    let mut prev_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if ('\u{0600}'..='\u{06ff}').contains(&ch) {
            // Deterministic hex encoding for Arabic letters so filesystem directory names stay ASCII-safe on all OSes.
            let code = u32::from(ch);
            if !slug.is_empty() && !prev_dash {
                slug.push('-');
            }
            slug.push_str(&format!("u{code:04x}"));
            prev_dash = false;
        } else if !slug.is_empty() && !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
        if slug.len() >= 56 {
            break;
        }
    }
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(1);
        format!("pack-{stamp}")
    } else {
        trimmed
    }
}

pub fn validate_pack_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 96 || !id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_')) {
        return Err("Invalid knowledge pack ID".into());
    }
    let mut comps = Path::new(id).components();
    if !matches!(comps.next(), Some(std::path::Component::Normal(_))) || comps.next().is_some() {
        return Err("Knowledge pack ID must be a single path segment".into());
    }
    Ok(())
}

fn current_iso_timestamp() -> String {
    let secs =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX)).unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0).map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()).unwrap_or_else(|| "1970-01-01T00:00:00Z".into())
}

pub fn create_knowledge_pack(
    profile_access: &AutomationRoots,
    ai_root: &Path,
    name: &str,
    description: &str,
    paths: &[String],
) -> Result<KnowledgePackSummary, String> {
    let trimmed_name = name.trim();
    if trimmed_name.is_empty() || trimmed_name.len() > 256 {
        return Err("Enter a knowledge pack name between 1 and 256 bytes".into());
    }
    if description.len() > 2048 {
        return Err("Knowledge pack description exceeds 2048 bytes".into());
    }
    if paths.is_empty() || paths.len() > MAX_PACK_FILES {
        return Err(format!("Select between 1 and {MAX_PACK_FILES} document files for a knowledge pack"));
    }

    let base_slug = slugify_pack_id(trimmed_name);
    let pack_id = crate::runtime::allocate_knowledge_pack_id(profile_access, ai_root, &base_slug)?;

    let mut all_chunks = Vec::new();
    let mut sources = Vec::new();
    for raw_path in paths {
        let clean = designcraft_ai_types::sanitize_user_path(raw_path);
        let path = Path::new(&clean);
        let bytes = read_local_regular_file(path, MAX_PACK_FILE_BYTES)?;
        let source_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| clean.clone());
        let (file_chunks, _, _) = ingest_document_bytes(&source_name, &bytes, false)?;
        sources.push(source_name);
        for chunk in file_chunks {
            if all_chunks.len() >= MAX_CHUNKS_PER_PACK {
                break;
            }
            all_chunks.push(chunk);
        }
    }

    let summary = KnowledgePackSummary {
        id: pack_id.clone(),
        name: trimmed_name.to_string(),
        description: description.trim().to_string(),
        file_count: sources.len(),
        chunk_count: all_chunks.len(),
        updated_at: current_iso_timestamp(),
        sources,
    };

    let manifest = PersistedPackManifest { schema_version: 1, summary: summary.clone() };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    let chunks_bytes = serde_json::to_vec_pretty(&all_chunks).map_err(|e| e.to_string())?;
    crate::runtime::persist_knowledge_pack_files(profile_access, ai_root, &pack_id, &manifest_bytes, &chunks_bytes)?;

    Ok(summary)
}

pub fn delete_knowledge_pack(profile_access: &AutomationRoots, ai_root: &Path, id: &str) -> Result<(), String> {
    crate::runtime::remove_knowledge_pack_dir(profile_access, ai_root, id)
}

pub fn list_knowledge_packs(profile_access: &AutomationRoots, ai_root: &Path) -> Result<Vec<KnowledgePackSummary>, String> {
    let ids = crate::runtime::list_knowledge_pack_dir_ids(profile_access, ai_root)?;
    let packs_root = ai_root.join("knowledge-packs");
    let mut summaries = Vec::new();
    for id_str in ids {
        let manifest_path = packs_root.join(&id_str).join("manifest.json");
        if profile_access.check(&manifest_path, Access::Read).is_err() {
            continue;
        }
        if let Ok(bytes) = designcraft_engine::io::read_capped(&manifest_path, 512 * 1024)
            && let Ok(manifest) = serde_json::from_slice::<PersistedPackManifest>(&bytes)
        {
            summaries.push(manifest.summary);
        }
    }
    summaries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
    Ok(summaries)
}

pub fn load_knowledge_pack_chunks(profile_access: Option<&AutomationRoots>, ai_root: &Path, id: &str) -> Result<Vec<KnowledgeChunk>, String> {
    validate_pack_id(id)?;
    let chunks_path = ai_root.join("knowledge-packs").join(id).join("chunks.json");
    match profile_access {
        Some(access) => access.check(&chunks_path, Access::Read)?,
        None => file_access::check_read(&chunks_path)?,
    }
    let bytes = designcraft_engine::io::read_capped(&chunks_path, 8 * 1024 * 1024).map_err(|e| format!("Knowledge pack '{id}': {e}"))?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err(format!("Knowledge pack '{id}' exceeds 8 MiB chunk storage limit"));
    }
    serde_json::from_slice::<Vec<KnowledgeChunk>>(&bytes).map_err(|e| format!("Knowledge pack '{id}' chunks: {e}"))
}

/// Build a ready-to-query [`KnowledgeIndex`] combining per-task attachments, active Knowledge Packs, and approved skills.
pub fn build_task_knowledge_index(
    profile_access: Option<&AutomationRoots>,
    ai_root: Option<&Path>,
    attachments: &[TaskAttachment],
    pack_ids: &[String],
    skills: &[SkillSnapshot],
) -> (KnowledgeIndex, Vec<ImageInput>, Vec<String>) {
    let mut index = KnowledgeIndex::new();
    let mut scanned_images = Vec::new();
    let mut warnings = Vec::new();

    match ingest_task_attachments(attachments) {
        Ok(bundle) => {
            index.extend_chunks(bundle.chunks);
            scanned_images = bundle.scanned_page_images;
            warnings.extend(bundle.warnings);
        }
        Err(err) => warnings.push(err),
    }

    if let Some(root) = ai_root {
        let mut unique_ids = BTreeSet::new();
        for id in pack_ids {
            let trimmed = id.trim();
            if !trimmed.is_empty() && unique_ids.insert(trimmed.to_string()) {
                match load_knowledge_pack_chunks(profile_access, root, trimmed) {
                    Ok(chunks) => index.extend_chunks(chunks),
                    Err(err) => warnings.push(err),
                }
            }
        }
    }

    index.index_approved_skills(skills);
    (index, scanned_images, warnings)
}

// ============================================================================
// Layer 3: Engine Command Catalog Lookup (`70+` commands + bilingual aliases)
// ============================================================================

fn bilingual_command_aliases(id: &str) -> &'static str {
    match id {
        "frame.create" => "انشاء اطار نص صورة شكل مستطيل text frame graphic frame rectangle box",
        "text.insert" | "text.replace" => "ادراج نص كتابة استبدال محتوى write text content paragraph",
        "type.char" => "تنسيق حروف خط حجم لون وزن عريض مائل font family size bold color character formatting",
        "type.para" => "تنسيق فقرة محاذاة اتجاه يمين يسار تباعد اسطر alignment direction rtl ltr leading paragraph",
        "type.diacritics" => "تشكيل حركات ضبط نص عربي diacritics tashkeel harakat vowelization",
        "swatch.create" => "اضافة لون عينة لونية باليت هوية بصرية add color swatch palette rgb cmyk hex",
        "style.define" | "style.apply" => "تعريف نمط فقرة نمط حروف تطبيق ستايل paragraph style character style",
        "object.style" => "تعبئة اطار حد شفافية ظل زوايا دائرية fill stroke corner radius drop shadow opacity",
        "object.arrange" => "ترتيب طبقات ارسال للخلف احضار للامام z-order send to back bring to front layer order",
        "object.transform" => "تحريك تغيير حجم تدوير احداثيات move resize rotate bounds rect",
        "object.qrCode" => "باركود رمز استجابة سريعة رابط qr code barcode vcard url",
        "math.insert" => "معادلة رياضية كسور جذور رموز علمية math equation formula latex",
        "toc.generate" => "جدول محتويات فهرس الكتاب ترقيم تلقائي table of contents index rtl",
        "table.insert" | "table.setCell" => "جدول صفوف اعمدة خلايا بيانات table grid rows columns cells",
        "file.place" | "frame.placeImage" => "ادراج صورة شعار ملف وضع اصل بصري place image logo asset reuseAsset",
        "layout.createAlternate" => "مقاسات بديلة تخطيط بديل سوشيل ميديا ستوري بنر alternate layout multi-format resize",
        "liquid.pageRule" | "liquid.object" => "تخطيط سائل اعادة تدفق تلقائي تثبيت حواف liquid layout scale recenter objectBased",
        "variables.insert" | "variables.define" => "متغيرات نصية رقم الصفحة عنوان الفصل running header text variable",
        "preflight.run" => "فحص الجودة نص فائض دقة الصور الهوامش preflight overset text bleed resolution check",
        _ => "",
    }
}

/// Search Layer 3 (Native Engine Command Catalog across `70+` commands + `production::catalog()` schemas).
pub fn lookup_engine_commands(query: &str, production_catalog: Option<&Value>, limit: usize) -> Vec<Value> {
    let cap = limit.clamp(1, 25);
    let mut prod_map: BTreeMap<String, Value> = BTreeMap::new();
    let fallback_cat;
    let cat_ref = match production_catalog {
        Some(c) => c,
        None => {
            fallback_cat = designcraft_engine::cmd::production::catalog();
            &fallback_cat
        }
    };
    if let Some(arr) = cat_ref.get("commands").and_then(Value::as_array) {
        for entry in arr {
            if let Some(id) = entry.get("command").and_then(Value::as_str) {
                prod_map.insert(id.to_string(), entry.clone());
            }
        }
    }

    let mut index = KnowledgeIndex::new();
    let mut spec_lookup: BTreeMap<String, Value> = BTreeMap::new();

    for spec in designcraft_engine::command_specs() {
        let in_prod = prod_map.get(spec.id);
        let aliases = bilingual_command_aliases(spec.id);
        let menu_str = spec.menu.join(" > ");
        let prod_desc = in_prod.and_then(|v| serde_json::to_string(v).ok()).unwrap_or_default();
        let searchable = format!("{} {} {} {} {} {}", spec.id, spec.label, menu_str, spec.params, aliases, prod_desc);
        index.push_chunk(KnowledgeChunk {
            chunk_id: format!("cmd:{}", spec.id),
            layer: KnowledgeLayer::EngineCommand,
            source_name: spec.id.to_string(),
            page_or_sheet: None,
            section_heading: Some(spec.label.to_string()),
            text: truncate_chars(&searchable, MAX_CHUNK_CHARS),
        });
        spec_lookup.insert(
            spec.id.to_string(),
            json!({
                "command": spec.id,
                "label": spec.label,
                "menu": spec.menu,
                "paramsDoc": spec.params,
                "allowedInProductionApply": in_prod.is_some(),
                "productionSchema": in_prod,
            }),
        );
    }

    // Also ensure any production catalog entry not in `command_specs()` is indexed.
    for (id, schema) in &prod_map {
        if !spec_lookup.contains_key(id) {
            let aliases = bilingual_command_aliases(id);
            let schema_text = serde_json::to_string(schema).unwrap_or_default();
            index.push_chunk(KnowledgeChunk {
                chunk_id: format!("cmd:{id}"),
                layer: KnowledgeLayer::EngineCommand,
                source_name: id.clone(),
                page_or_sheet: None,
                section_heading: Some(id.clone()),
                text: truncate_chars(&format!("{id} {aliases} {schema_text}"), MAX_CHUNK_CHARS),
            });
            spec_lookup.insert(
                id.clone(),
                json!({
                    "command": id,
                    "label": id,
                    "allowedInProductionApply": true,
                    "productionSchema": schema,
                }),
            );
        }
    }

    if query.trim().is_empty() {
        return prod_map.keys().take(cap).filter_map(|id| spec_lookup.get(id).cloned()).collect();
    }

    let hits = index.search(query, cap.saturating_mul(2));
    let mut results = Vec::new();
    // Prioritize commands allowed in `production.apply` first when scores are close.
    let mut allowed_hits = Vec::new();
    let mut other_hits = Vec::new();
    for hit in hits {
        if let Some(entry) = spec_lookup.get(&hit.chunk.source_name) {
            if entry.get("allowedInProductionApply").and_then(Value::as_bool) == Some(true) {
                allowed_hits.push(entry.clone());
            } else {
                other_hits.push(entry.clone());
            }
        }
    }
    for item in allowed_hits.into_iter().chain(other_hits) {
        if results.len() >= cap {
            break;
        }
        results.push(item);
    }
    results
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn arabic_bm25_matches_presentation_forms_prefixes_and_plurals() {
        let mut idx = KnowledgeIndex::new();
        idx.push_chunk(KnowledgeChunk {
            chunk_id: "c1".into(),
            layer: KnowledgeLayer::ProjectOrPack,
            source_name: "brand-guide.pdf".into(),
            page_or_sheet: Some("page 4".into()),
            section_heading: Some("الهوية اللونية".into()),
            // Uses Arabic Presentation Forms for "الألوان" / "الشركة" + tashkeel
            text: "تَعتَمِدُ \u{FE8D}\u{FEDF}\u{FE8D}\u{FEDF}\u{FEED}\u{FE8D}\u{FEE5} الرسمية في المطبوعات التجارية على الأزرق الداكن والذهبي.".into(),
        });
        idx.push_chunk(KnowledgeChunk {
            chunk_id: "c2".into(),
            layer: KnowledgeLayer::ProjectOrPack,
            source_name: "shipping.pdf".into(),
            page_or_sheet: Some("page 1".into()),
            section_heading: None,
            text: "مواعيد الشحن والتوصيل داخل المدن الرئيسية خلال يومين.".into(),
        });

        let hits = idx.search("بالالوان الرسميه للمطبوعه التجاريه", 3);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].chunk.chunk_id, "c1");
        assert_eq!(hits[0].chunk.page_or_sheet.as_deref(), Some("page 4"));
    }

    #[test]
    fn knowledge_pack_roundtrip_persists_and_deletes_cleanly() {
        let root = tempfile::tempdir().unwrap();
        let docs = tempfile::tempdir().unwrap();
        let md_path = docs.path().join("pricing.csv");
        std::fs::write(&md_path, "Plan,Price,Audience\nPersonal,49 SAR,Individuals\nEnterprise,299 SAR,Corporations\n").unwrap();

        let access = AutomationRoots::new(&[root.path(), docs.path()], &[root.path()]).unwrap().unwrap();
        let summary = file_access::confine(Some(&access), || {
            create_knowledge_pack(&access, root.path(), "حزمة الأسعار 2026", "جدول باقات الأفراد والشركات", &[md_path.to_string_lossy().into_owned()])
        })
        .unwrap();
        assert_eq!(summary.file_count, 1);
        assert!(summary.chunk_count >= 1);

        let listed = list_knowledge_packs(&access, root.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, summary.id);

        let chunks = load_knowledge_pack_chunks(Some(&access), root.path(), &summary.id).unwrap();
        let mut idx = KnowledgeIndex::new();
        idx.extend_chunks(chunks);
        let hits = idx.search("Enterprise 299 SAR", 3);
        assert!(!hits.is_empty());

        delete_knowledge_pack(&access, root.path(), &summary.id).unwrap();
        assert!(list_knowledge_packs(&access, root.path()).unwrap().is_empty());
    }

    #[test]
    fn lookup_engine_commands_finds_arabic_and_english_capabilities() {
        let toc_hits = lookup_engine_commands("جدول محتويات فهرس", None, 5);
        assert!(toc_hits.iter().any(|v| v["command"] == "toc.generate"));

        let alt_hits = lookup_engine_commands("alternate layout multi-format", None, 5);
        assert!(alt_hits.iter().any(|v| v["command"] == "layout.createAlternate"));
    }
}
