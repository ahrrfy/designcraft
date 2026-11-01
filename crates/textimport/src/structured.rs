//! Structured document, multi-sheet workbook, and CSV/TSV extraction for AI and data workflows.

use std::collections::BTreeSet;

use designcraft_doc::{ENDNOTE_REF, FOOTNOTE_REF, Story, Table};

use crate::{DATA_MERGE_MAX_COLS, DATA_MERGE_MAX_ROWS, ImportError, import, tagged};

/// Maximum raw byte size accepted for structured CSV/TSV text extraction (128 MiB).
const MAX_DELIMITED_BYTES: usize = 128 * 1024 * 1024;

/// Extracted structured table with column headers and body rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StructuredTableExtract {
    pub name: String,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Extracted structured representation of an Office, Markdown, plain-text, or delimited file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StructuredDocumentExtract {
    pub plain_text: String,
    pub markdown: String,
    pub tables: Vec<StructuredTableExtract>,
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
}

/// Return all worksheet names in `.xlsx` workbook order.
pub fn xlsx_sheet_names(bytes: &[u8]) -> Result<Vec<String>, ImportError> {
    crate::xlsx::sheet_names(bytes)
}

/// Parse UTF-8 or UTF-16 delimited text (with optional BOM) using RFC-4180 quote rules,
/// bounded by [`DATA_MERGE_MAX_ROWS`] and [`DATA_MERGE_MAX_COLS`].
pub fn csv_records(bytes: &[u8], delimiter: u8) -> Result<Vec<Vec<String>>, ImportError> {
    if bytes.len() > MAX_DELIMITED_BYTES {
        return Err(ImportError::Corrupt(format!("delimited file exceeds {} MiB limit", MAX_DELIMITED_BYTES / (1024 * 1024))));
    }
    let decoded = tagged::decode(bytes);
    let text = decoded.trim_start_matches('\u{FEFF}');
    parse_rfc4180(text, char::from(delimiter))
}

/// Extract structured plain text, Markdown, tables, and footnotes/endnotes from `bytes`.
pub fn extract_structured_text(name: &str, bytes: &[u8]) -> Result<StructuredDocumentExtract, ImportError> {
    let lower = name.trim().to_ascii_lowercase();
    if lower.ends_with(".xlsx") {
        return extract_xlsx(bytes);
    }
    if lower.ends_with(".csv") || lower.ends_with(".tsv") {
        let delimiter = if lower.ends_with(".tsv") { b'\t' } else { detect_csv_delimiter(bytes) };
        return extract_delimited(name, bytes, delimiter);
    }

    let imported = import(name, bytes)?;
    Ok(extract_from_story(&imported.story, imported.warnings))
}

fn extract_xlsx(bytes: &[u8]) -> Result<StructuredDocumentExtract, ImportError> {
    let sheet_names = xlsx_sheet_names(bytes)?;
    let mut tables = Vec::with_capacity(sheet_names.len());
    let mut md_sections = Vec::with_capacity(sheet_names.len());
    let mut plain_sections = Vec::with_capacity(sheet_names.len());
    let mut warnings = Vec::new();

    for sheet in sheet_names {
        match crate::xlsx_records(bytes, Some(&sheet)) {
            Ok(records) => {
                let Some((first, rest)) = records.split_first() else {
                    continue;
                };
                let headers: Vec<String> = first.iter().map(|s| strip_pua(s)).collect();
                let rows: Vec<Vec<String>> = rest.iter().map(|r| r.iter().map(|s| strip_pua(s)).collect()).collect();
                let clean_sheet = strip_pua(&sheet);
                md_sections.push(format!("## {clean_sheet}\n{}", render_markdown_table(&headers, &rows)));
                plain_sections.push(format!("{clean_sheet}\n{}", render_plain_table(&headers, &rows)));
                tables.push(StructuredTableExtract { name: clean_sheet, headers, rows });
            }
            Err(ImportError::Corrupt(msg)) if msg == "the worksheet is empty" || msg == "the worksheet has no header" => {
                warnings.push(format!("{sheet}: {msg}"));
            }
            Err(e) => return Err(e),
        }
    }

    Ok(StructuredDocumentExtract { plain_text: plain_sections.join("\n\n"), markdown: md_sections.join("\n\n"), tables, notes: Vec::new(), warnings })
}

fn detect_csv_delimiter(bytes: &[u8]) -> u8 {
    let head = tagged::decode(&bytes[..bytes.len().min(8192)]);
    let text = head.trim_start_matches('\u{FEFF}');
    let mut in_quotes = false;
    let mut commas = 0usize;
    let mut semis = 0usize;
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        if in_quotes {
            if c == '"' {
                if it.peek() == Some(&'"') {
                    it.next();
                } else {
                    in_quotes = false;
                }
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => commas += 1,
                ';' => semis += 1,
                '\r' | '\n' => break,
                _ => {}
            }
        }
    }
    if semis > 0 && commas == 0 { b';' } else { b',' }
}

fn extract_delimited(name: &str, bytes: &[u8], delimiter: u8) -> Result<StructuredDocumentExtract, ImportError> {
    let records = csv_records(bytes, delimiter)?;
    let Some((first, rest)) = records.split_first() else {
        return Ok(StructuredDocumentExtract::default());
    };
    let headers: Vec<String> = first.iter().map(|s| strip_pua(s)).collect();
    let rows: Vec<Vec<String>> = rest.iter().map(|r| r.iter().map(|s| strip_pua(s)).collect()).collect();
    let table_name = strip_pua(table_name_from_path(name));
    let markdown = render_markdown_table(&headers, &rows);
    let plain_text = render_plain_table(&headers, &rows);
    let tables = vec![StructuredTableExtract { name: table_name, headers, rows }];
    Ok(StructuredDocumentExtract { plain_text, markdown, tables, notes: Vec::new(), warnings: Vec::new() })
}

fn table_name_from_path(name: &str) -> &str {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    match base.rsplit_once('.') {
        Some((stem, _)) if !stem.trim().is_empty() => stem.trim(),
        _ if !base.is_empty() => base,
        _ => "Table 1",
    }
}

fn extract_from_story(story: &Story, warnings: Vec<String>) -> StructuredDocumentExtract {
    let mut tables = Vec::new();
    let mut seen_tables = BTreeSet::new();
    let mut plain_blocks: Vec<String> = Vec::new();
    let mut md_blocks: Vec<String> = Vec::new();
    let mut footnote_idx = 0usize;
    let mut endnote_idx = 0usize;
    let total_notes = story.notes.len().saturating_add(story.endnotes.len());

    for (pi, range) in story.para_ranges().into_iter().enumerate() {
        if let Some(table_id) = story.paras.get(pi).and_then(|pf| pf.table)
            && let Some(tbl) = story.tables.get(&table_id)
        {
            seen_tables.insert(table_id);
            let label = format!("Table {}", tables.len() + 1);
            let extracted = table_to_extract(&label, tbl);
            plain_blocks.push(render_plain_table(&extracted.headers, &extracted.rows));
            md_blocks.push(render_markdown_table(&extracted.headers, &extracted.rows));
            tables.push(extracted);
            continue;
        }

        let slice = story.text.get(range).unwrap_or("");
        let mut plain_line = String::with_capacity(slice.len());
        let mut md_line = String::with_capacity(slice.len());
        for ch in slice.chars() {
            if ch == FOOTNOTE_REF {
                footnote_idx = footnote_idx.saturating_add(1);
                let num = footnote_idx.min(total_notes.max(1));
                md_line.push_str(&format!("[^{num}]"));
            } else if ch == ENDNOTE_REF {
                endnote_idx = endnote_idx.saturating_add(1);
                let num = story.notes.len().saturating_add(endnote_idx);
                md_line.push_str(&format!("[^{num}]"));
            } else if !is_pua(ch) {
                plain_line.push(ch);
                md_line.push(ch);
            }
        }
        plain_blocks.push(plain_line);
        md_blocks.push(md_line);
    }

    // Emit any residual tables in `story.tables` that were not anchored via `para.table`.
    for (&table_id, tbl) in &story.tables {
        if seen_tables.insert(table_id) {
            let label = format!("Table {}", tables.len() + 1);
            let extracted = table_to_extract(&label, tbl);
            plain_blocks.push(render_plain_table(&extracted.headers, &extracted.rows));
            md_blocks.push(render_markdown_table(&extracted.headers, &extracted.rows));
            tables.push(extracted);
        }
    }

    let mut notes = Vec::with_capacity(total_notes);
    for note in story.notes.iter().chain(story.endnotes.iter()) {
        let cleaned = strip_pua(&note.text.text).trim().to_string();
        if !cleaned.is_empty() {
            notes.push(cleaned);
        }
    }

    let mut plain_text = strip_pua(&plain_blocks.join("\n")).trim_matches('\n').to_string();
    let mut markdown = strip_pua(&md_blocks.join("\n\n")).trim().to_string();

    if !notes.is_empty() {
        let mut plain_notes = String::from("Notes:");
        let mut md_notes = String::from("## Notes");
        for (i, n) in notes.iter().enumerate() {
            let idx = i + 1;
            plain_notes.push_str(&format!("\n{idx}. {n}"));
            md_notes.push_str(&format!("\n[^{idx}]: {n}"));
        }
        if plain_text.is_empty() {
            plain_text = plain_notes;
        } else {
            plain_text.push_str("\n\n");
            plain_text.push_str(&plain_notes);
        }
        if markdown.is_empty() {
            markdown = md_notes;
        } else {
            markdown.push_str("\n\n");
            markdown.push_str(&md_notes);
        }
    }

    StructuredDocumentExtract { plain_text, markdown, tables, notes, warnings }
}

fn table_to_extract(name: &str, tbl: &Table) -> StructuredTableExtract {
    let nrows = tbl.nrows();
    let ncols = tbl.ncols();
    if nrows == 0 || ncols == 0 {
        return StructuredTableExtract { name: name.into(), headers: Vec::new(), rows: Vec::new() };
    }
    let mut all_rows = Vec::with_capacity(nrows);
    for r in 0..nrows {
        let mut row = Vec::with_capacity(ncols);
        for c in 0..ncols {
            let cell_text = tbl.cell(r, c).map(|cell| strip_pua(&cell.text.text)).unwrap_or_default();
            row.push(cell_text);
        }
        all_rows.push(row);
    }
    let headers = all_rows.first().cloned().unwrap_or_default();
    let rows = all_rows.get(1..).map(<[Vec<String>]>::to_vec).unwrap_or_default();
    StructuredTableExtract { name: name.into(), headers, rows }
}

fn render_markdown_table(headers: &[String], rows: &[Vec<String>]) -> String {
    if headers.is_empty() {
        return String::new();
    }
    let ncols = headers.len();
    let mut out = String::new();
    out.push('|');
    for h in headers {
        out.push(' ');
        out.push_str(&escape_md_cell(h));
        out.push_str(" |");
    }
    out.push('\n');
    out.push('|');
    for _ in 0..ncols {
        out.push_str(" --- |");
    }
    for row in rows {
        out.push('\n');
        out.push('|');
        for c in 0..ncols {
            let val = row.get(c).map_or("", String::as_str);
            out.push(' ');
            out.push_str(&escape_md_cell(val));
            out.push_str(" |");
        }
    }
    out
}

fn escape_md_cell(s: &str) -> String {
    s.replace("\r\n", " ").replace(['\r', '\n'], " ").replace('|', "\\|").trim().to_string()
}

fn render_plain_table(headers: &[String], rows: &[Vec<String>]) -> String {
    if headers.is_empty() {
        return String::new();
    }
    let clean = |s: &str| s.replace("\r\n", " ").replace(['\r', '\n', '\t'], " ");
    let mut lines = Vec::with_capacity(rows.len().saturating_add(1));
    lines.push(headers.iter().map(|h| clean(h)).collect::<Vec<_>>().join("\t"));
    for row in rows {
        lines.push(row.iter().map(|v| clean(v)).collect::<Vec<_>>().join("\t"));
    }
    lines.join("\n")
}

fn is_pua(c: char) -> bool {
    ('\u{E000}'..='\u{F8FF}').contains(&c)
}

fn strip_pua(s: &str) -> String {
    s.chars().filter(|&c| !is_pua(c)).collect()
}

fn parse_rfc4180(text: &str, delim: char) -> Result<Vec<Vec<String>>, ImportError> {
    let mut records: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut field_started = false;
    let mut row_had_quotes = false;
    let mut it = text.chars().peekable();

    let push_row = |records: &mut Vec<Vec<String>>, mut r: Vec<String>, had_quotes: bool| -> Result<(), ImportError> {
        if r.len() > DATA_MERGE_MAX_COLS {
            return Err(ImportError::Corrupt(format!("the file has {} columns; the limit is {DATA_MERGE_MAX_COLS}", r.len())));
        }
        if !had_quotes && r.iter().all(String::is_empty) {
            return Ok(());
        }
        if records.len() > DATA_MERGE_MAX_ROWS {
            return Err(ImportError::Corrupt(format!("the file has more than {DATA_MERGE_MAX_ROWS} data rows")));
        }
        if let Some(header_len) = records.first().map(Vec::len)
            && r.len() < header_len
        {
            r.resize(header_len, String::new());
        }
        records.push(r);
        Ok(())
    };

    while let Some(c) = it.next() {
        if in_quotes {
            if c == '"' {
                if it.peek() == Some(&'"') {
                    it.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else if c == '\r' {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                field.push('\n');
            } else {
                field.push(c);
            }
            continue;
        }

        if !field_started && c == '"' {
            in_quotes = true;
            field_started = true;
            row_had_quotes = true;
            continue;
        }

        if c == delim {
            if row.len() >= DATA_MERGE_MAX_COLS {
                return Err(ImportError::Corrupt(format!("the file exceeds the {DATA_MERGE_MAX_COLS}-column limit")));
            }
            row.push(std::mem::take(&mut field));
            field_started = false;
        } else if c == '\r' || c == '\n' {
            if c == '\r' && it.peek() == Some(&'\n') {
                it.next();
            }
            row.push(std::mem::take(&mut field));
            push_row(&mut records, std::mem::take(&mut row), row_had_quotes)?;
            field_started = false;
            row_had_quotes = false;
        } else {
            field.push(c);
            field_started = true;
        }
    }

    if in_quotes {
        return Err(ImportError::Corrupt("unclosed quoted field in delimited text".into()));
    }
    if field_started || !row.is_empty() {
        row.push(field);
        push_row(&mut records, row, row_had_quotes)?;
    }

    Ok(records)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn zip_parts(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut z = zip::ZipWriter::new(&mut buf);
            let opts = zip::write::SimpleFileOptions::default();
            for &(name, body) in parts {
                z.start_file(name, opts).unwrap();
                z.write_all(body.as_bytes()).unwrap();
            }
            z.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extracts_all_sheets_from_multi_sheet_xlsx_in_english_and_arabic() {
        let xlsx = zip_parts(&[
            (
                "xl/workbook.xml",
                r#"<workbook xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Overview" sheetId="1" r:id="rId1"/><sheet name="الأسعار" sheetId="2" r:id="rId2"/><sheet name="EmptySheet" sheetId="3" r:id="rId3"/></sheets></workbook>"#,
            ),
            (
                "xl/_rels/workbook.xml.rels",
                r#"<Relationships><Relationship Id="rId1" Type="x" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="x" Target="worksheets/sheet2.xml"/><Relationship Id="rId3" Type="x" Target="worksheets/sheet3.xml"/></Relationships>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Metric</t></is></c><c r="B1" t="inlineStr"><is><t>Value</t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>Pages</t></is></c><c r="B2"><v>24</v></c></row></sheetData></worksheet>"#,
            ),
            (
                "xl/worksheets/sheet2.xml",
                r#"<worksheet><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>المنتج</t></is></c><c r="B1" t="inlineStr"><is><t>السعر</t></is></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>كتاب فني</t></is></c><c r="B2"><v>150</v></c></row></sheetData></worksheet>"#,
            ),
            ("xl/worksheets/sheet3.xml", r#"<worksheet><sheetData/></worksheet>"#),
        ]);

        let names = xlsx_sheet_names(&xlsx).unwrap();
        assert_eq!(names, vec!["Overview", "الأسعار", "EmptySheet"]);

        let out = extract_structured_text("catalog.xlsx", &xlsx).unwrap();
        assert_eq!(out.tables.len(), 2);
        assert_eq!(out.tables[0].name, "Overview");
        assert_eq!(out.tables[0].headers, vec!["Metric", "Value"]);
        assert_eq!(out.tables[0].rows, vec![vec!["Pages", "24"]]);

        assert_eq!(out.tables[1].name, "الأسعار");
        assert_eq!(out.tables[1].headers, vec!["المنتج", "السعر"]);
        assert_eq!(out.tables[1].rows, vec![vec!["كتاب فني", "150"]]);

        assert!(out.markdown.contains("## Overview\n| Metric | Value |"));
        assert!(out.markdown.contains("## الأسعار\n| المنتج | السعر |"));
        assert!(out.plain_text.contains("كتاب فني\t150"));
        assert_eq!(out.warnings.len(), 1);
    }

    #[test]
    fn extracts_docx_body_inline_table_and_footnote_without_private_use_markers() {
        let docx = zip_parts(&[
            (
                "word/document.xml",
                r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
                    <w:body>
                        <w:p><w:r><w:t>مقدمة التقرير السنوي</w:t></w:r><w:r><w:footnoteReference w:id="1"/></w:r></w:p>
                        <w:tbl>
                            <w:tr>
                                <w:tc><w:p><w:r><w:t>القسم</w:t></w:r></w:p></w:tc>
                                <w:tc><w:p><w:r><w:t>الإنجاز</w:t></w:r></w:p></w:tc>
                            </w:tr>
                            <w:tr>
                                <w:tc><w:p><w:r><w:t>التصميم</w:t></w:r></w:p></w:tc>
                                <w:tc><w:p><w:r><w:t>مكتمل</w:t></w:r></w:p></w:tc>
                            </w:tr>
                        </w:tbl>
                        <w:p><w:r><w:t>الخاتمة التنفيذية</w:t></w:r></w:p>
                    </w:body>
                </w:document>"#,
            ),
            (
                "word/footnotes.xml",
                r#"<w:footnotes xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
                    <w:footnote w:id="1"><w:p><w:r><w:t>هامش توضيحي معتمد</w:t></w:r></w:p></w:footnote>
                </w:footnotes>"#,
            ),
        ]);

        let out = extract_structured_text("report.docx", &docx).unwrap();
        assert_eq!(out.tables.len(), 1);
        assert_eq!(out.tables[0].headers, vec!["القسم", "الإنجاز"]);
        assert_eq!(out.tables[0].rows, vec![vec!["التصميم", "مكتمل"]]);
        assert_eq!(out.notes, vec!["هامش توضيحي معتمد"]);

        assert!(out.markdown.contains("مقدمة التقرير السنوي[^1]"));
        assert!(out.markdown.contains("| القسم | الإنجاز |"));
        assert!(out.markdown.contains("| التصميم | مكتمل |"));
        assert!(out.markdown.contains("الخاتمة التنفيذية"));
        assert!(out.markdown.contains("## Notes\n[^1]: هامش توضيحي معتمد"));

        assert!(out.plain_text.contains("مقدمة التقرير السنوي"));
        assert!(out.plain_text.contains("القسم\tالإنجاز\nالتصميم\tمكتمل"));
        assert!(out.plain_text.contains("Notes:\n1. هامش توضيحي معتمد"));

        assert!(!out.plain_text.chars().any(is_pua));
        assert!(!out.markdown.chars().any(is_pua));
    }

    #[test]
    fn parses_csv_with_quotes_bom_semicolon_autodetect_and_tsv() {
        let mut csv_bytes = vec![0xEF, 0xBB, 0xBF];
        csv_bytes.extend_from_slice("الاسم,الوصف,السعر\r\n\"أحمد، علي\",\"طبعة \"\"فاخرة\"\" محدودة\",250\r\nسارة,نسخة رقمية,95\r\n".as_bytes());

        let out_csv = extract_structured_text("orders.csv", &csv_bytes).unwrap();
        assert_eq!(out_csv.tables.len(), 1);
        assert_eq!(out_csv.tables[0].name, "orders");
        assert_eq!(out_csv.tables[0].headers, vec!["الاسم", "الوصف", "السعر"]);
        assert_eq!(
            out_csv.tables[0].rows,
            vec![
                vec!["أحمد، علي".to_string(), "طبعة \"فاخرة\" محدودة".to_string(), "250".to_string()],
                vec!["سارة".to_string(), "نسخة رقمية".to_string(), "95".to_string()],
            ]
        );
        assert!(out_csv.markdown.contains("| الاسم | الوصف | السعر |"));

        // Semicolon CSV auto-detection when header uses `;` and zero unquoted `,`:
        let semi_csv = b"City;Country;Population\n\"Riyadh, KSA\";Saudi Arabia;7600000\n";
        let out_semi = extract_structured_text("cities.csv", semi_csv).unwrap();
        assert_eq!(out_semi.tables[0].headers, vec!["City", "Country", "Population"]);
        assert_eq!(out_semi.tables[0].rows, vec![vec!["Riyadh, KSA", "Saudi Arabia", "7600000"]]);

        // TSV:
        let tsv = "Code\tTitle\tPages\nDC-01\tDesignCraft Guide\t128\n";
        let out_tsv = extract_structured_text("guide.tsv", tsv.as_bytes()).unwrap();
        assert_eq!(out_tsv.tables[0].headers, vec!["Code", "Title", "Pages"]);
        assert_eq!(out_tsv.tables[0].rows, vec![vec!["DC-01", "DesignCraft Guide", "128"]]);
    }
}
