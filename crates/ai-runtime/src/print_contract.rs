//! Explicit, bounded production requirements. Prompt prose is data, never permission to edit.
use designcraft_ai_types::{DesignProfile, EngineAction};
use designcraft_engine::DocState;
use serde::Serialize;
use serde_json::{Value, json};

const PT_PER_MM: f64 = 72.0 / 25.4;
const MAX_SCRIPT_BYTES: usize = designcraft_ai_types::protocol::MAX_BRIEF_BYTES;
const MAX_SPEC_LINE: usize = 4096;

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct PrintContract {
    pub page_mm: Option<[f64; 2]>,
    /// top, bottom, inside, outside, matching native file.new.
    pub margins_mm: Option<[f64; 4]>,
    pub bleed_mm: Option<f64>,
    pub columns: Option<u32>,
    pub gutter_mm: Option<f64>,
    pub column_width_mm: Option<f64>,
    pub baseline_mm: Option<f64>,
    pub min_ppi: Option<f64>,
    pub pdf_x4: bool,
    pub crop_offset_mm: Option<f64>,
    pub english_fonts: Vec<String>,
    pub arabic_fonts: Vec<String>,
    pub unsupported: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AvailablePrintFont {
    pub family: String,
    pub regular: String,
    pub heading: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct PrintFonts {
    pub english: Option<AvailablePrintFont>,
    pub arabic: Option<AvailablePrintFont>,
    pub notices: Vec<String>,
}

impl PrintContract {
    /// Read only labelled global specifications, never infer geometry from an illustration.
    pub(crate) fn from_prompt(prompt: &str) -> Result<Option<Self>, String> {
        if prompt.len() > MAX_SCRIPT_BYTES {
            return Err("Print specification exceeds the 1-MiB script limit".into());
        }
        let mut out = Self::default();
        let mut found = false;
        let mut page_content = false;
        for line in prompt.lines() {
            let lower = line.to_lowercase();
            let heading = lower.trim_start_matches('#').trim_start();
            if lower.starts_with('#') && (heading.starts_with("page ") || heading.starts_with("صفحة ")) {
                page_content = true;
            }
            let final_export = lower.contains("prepress preflight export:");
            let spec = (!page_content).then(|| specification(line)).flatten();
            let labelled = spec.as_ref().is_some_and(|(key, _)| recognized_spec(key));
            if !labelled && !final_export {
                continue;
            }
            if line.len() > MAX_SPEC_LINE {
                return Err("A print specification line exceeds the 4096-byte limit".into());
            }
            if final_export {
                out.read_export(line)?;
                found = true;
                continue;
            }
            let Some((key, value)) = spec else { continue };
            let value = clean(value);
            match key.as_str() {
                "trim size" | "page size" => {
                    let size = if value.to_lowercase().contains("a4") {
                        if value.to_lowercase().contains("landscape") { [297.0, 210.0] } else { [210.0, 297.0] }
                    } else {
                        let numbers = numbers(&value)?;
                        let Some((&width, tail)) = numbers.split_first() else { return Err("Trim Size needs explicit dimensions in mm".into()) };
                        let Some(&height) = tail.first() else { return Err("Trim Size needs two dimensions in mm".into()) };
                        require_mm(&value, "Trim Size")?;
                        [bounded(width, 1.0, 5486.4, "page width")?, bounded(height, 1.0, 5486.4, "page height")?]
                    };
                    put(&mut out.page_mm, size, "Trim Size")?;
                }
                "bleed" => {
                    require_mm(&value, "Bleed")?;
                    let amount = first_number(&value, "Bleed")?;
                    put(&mut out.bleed_mm, bounded(amount, 0.0, 100.0, "bleed")?, "Bleed")?;
                }
                "safe margin / live area" | "safe margins" | "margins" => {
                    require_mm(&value, "Margins")?;
                    let values = numbers(&value)?;
                    let Some(&outer) = values.first() else { return Err("Margins need explicit dimensions".into()) };
                    let inside = match values.as_slice() {
                        [_] => outer,
                        [_, inner] if value.to_lowercase().contains("inside")
                            && ["top", "bottom", "outside"].iter().all(|edge| value.to_lowercase().contains(edge)) => *inner,
                        _ => return Err("Margins must specify one uniform mm value or grouped top/bottom/outside followed by inside mm; use explicit native settings for other edge combinations".into()),
                    };
                    bounded(outer, 0.0, 100.0, "margin")?;
                    bounded(inside, 0.0, 100.0, "inside margin")?;
                    put(&mut out.margins_mm, [outer, outer, inside, outer], "Margins")?;
                }
                "grid system" => out.read_grid(&value)?,
                "english primary font" => put_fonts(&mut out.english_fonts, font_candidates(&value)?, "English fonts")?,
                "arabic primary font" => put_fonts(&mut out.arabic_fonts, font_candidates(&value)?, "Arabic fonts")?,
                "color profile" | "colour profile" => {
                    out.unsupported.push(format!("Requested ICC/color profiles require explicit press setup; the automatic PDF/X export uses one active CMYK output intent and cannot apply separate cover/interior profiles: {value}"));
                }
                "image resolution" | "effective image resolution" => out.read_resolution(&value)?,
                "pdf standard" | "export standard" | "prepress preflight export" => out.read_export(&value)?,
                _ => continue,
            }
            found = true;
        }
        if !found {
            return Ok(None);
        }
        out.validate_supported()?;
        Ok(Some(out))
    }

    fn read_grid(&mut self, value: &str) -> Result<(), String> {
        let lower = value.to_lowercase();
        let column = lower.find("-column").or_else(|| lower.find(" column"));
        if let Some(position) = column {
            let count = preceding_number(&lower, position, "column count")?;
            if count.fract() != 0.0 {
                return Err("Print column count must be a whole number".into());
            }
            put(&mut self.columns, bounded(count, 1.0, 216.0, "column count")? as u32, "column count")?;
        }
        for (label, target) in [("column width:", &mut self.column_width_mm), ("gutter:", &mut self.gutter_mm)] {
            if let Some(position) = lower.find(label) {
                let tail = lower.get(position.saturating_add(label.len())..).ok_or("Invalid grid specification")?;
                require_mm(tail, label)?;
                let n = bounded(first_number(tail, label)?, 0.0, 1000.0, label)?;
                put(target, n, label)?;
            }
        }
        if let Some(position) = lower.find("baseline") {
            let before = lower.get(..position).ok_or("Invalid baseline specification")?;
            require_mm(before, "baseline")?;
            let increment = preceding_number(&lower, position, "baseline")?;
            put(&mut self.baseline_mm, bounded(increment, 0.1, 100.0, "baseline")?, "baseline")?;
        }
        if self.columns.is_none() && self.baseline_mm.is_none() {
            return Err("Grid System needs an explicit column count or baseline increment".into());
        }
        Ok(())
    }

    fn read_resolution(&mut self, value: &str) -> Result<(), String> {
        let lower = value.to_lowercase();
        let position = lower.find("ppi").or_else(|| lower.find("dpi")).ok_or("Image resolution must use PPI or DPI")?;
        let amount = bounded(preceding_number(&lower, position, "image resolution")?, 36.0, 2400.0, "image resolution")?;
        put(&mut self.min_ppi, amount, "image resolution")
    }

    fn read_export(&mut self, value: &str) -> Result<(), String> {
        let lower = clean(value).to_lowercase();
        if lower.contains("pdf/x-4") || lower.contains("pdf/x–4") {
            self.pdf_x4 = true;
        } else if lower.contains("pdf/x") || lower.contains("pdf/a") {
            self.unsupported.push("Requested PDF conformance standard is unsupported by automatic production export".into());
        }
        if let Some(position) = lower.find("crop marks offset") {
            let tail = lower.get(position..).ok_or("Invalid crop marks specification")?;
            require_mm(tail, "crop marks offset")?;
            put(&mut self.crop_offset_mm, bounded(first_number(tail, "crop marks offset")?, 0.0, 100.0, "crop marks offset")?, "crop marks offset")?;
        }
        if lower.contains("effective image resolution") || lower.contains("minimum image resolution") {
            let position = lower.find("image resolution").ok_or("Invalid image resolution specification")?;
            self.read_resolution(lower.get(position..).ok_or("Invalid image resolution specification")?)?;
        }
        if let Some(position) = lower.find("bleed") {
            let before = lower.get(..position).ok_or("Invalid export bleed specification")?;
            if before.contains("mm") {
                let amount = preceding_number(&lower, position, "export bleed")?;
                put(&mut self.bleed_mm, bounded(amount, 0.0, 100.0, "bleed")?, "Bleed")?;
            }
        }
        Ok(())
    }

    /// Bounds errors block purchases; unsatisfied final press requirements remain explicit warnings.
    pub(crate) fn validate_supported(&self) -> Result<(), String> {
        if let (Some([width, height]), Some([top, bottom, inside, outside])) = (self.page_mm, self.margins_mm) {
            if top + bottom >= height || inside + outside >= width {
                return Err("Requested print margins leave no live page area".into());
            }
            if let (Some(columns), Some(gutter)) = (self.columns, self.gutter_mm)
                && f64::from(columns.saturating_sub(1)) * gutter >= width - inside - outside
            {
                return Err("Requested column gutters leave no live column area".into());
            }
        }
        Ok(())
    }

    pub(crate) fn warnings(&self) -> Vec<String> {
        let mut warnings = self.unsupported.clone();
        if let (Some([width, _]), Some([_, _, inside, outside]), Some(columns), Some(gutter), Some(column_width)) =
            (self.page_mm, self.margins_mm, self.columns, self.gutter_mm, self.column_width_mm)
        {
            let available = width - inside - outside;
            let requested = f64::from(columns) * column_width + f64::from(columns.saturating_sub(1)) * gutter;
            if (available - requested).abs() > 0.1 {
                warnings.push(format!("Requested column width {column_width} mm conflicts with the page margins and gutters: {columns} columns span {requested} mm within {available} mm; native equal-width columns use {} mm. Confirm the grid before final print approval", (available - f64::from(columns.saturating_sub(1)) * gutter) / f64::from(columns)));
            }
        }
        warnings
    }

    /// A proof may be exported with warnings; it must never be labelled final print-ready.
    pub(crate) fn validate_final_export(&self) -> Result<(), String> {
        self.validate_supported()?;
        let warnings = self.warnings();
        if !warnings.is_empty() {
            return Err(format!("Final print requirements remain unresolved: {}", warnings.join("; ")));
        }
        Ok(())
    }

    /// Only for creation of a new private document. Existing documents use validate_existing.
    pub(crate) fn document_params(&self, profile: Option<&DesignProfile>, pages: u32) -> Result<Value, String> {
        self.validate_supported()?;
        if let Some(profile) = profile {
            profile.validate()?;
        }
        let mut params = crate::design::document_params(profile, pages);
        if let Some([width, height]) = self.page_mm {
            params["width"] = json!(width * PT_PER_MM);
            params["height"] = json!(height * PT_PER_MM);
            if let Some(object) = params.as_object_mut() {
                object.remove("preset");
            }
        }
        if let Some([top, bottom, inside, outside]) = self.margins_mm {
            params["margins"] = json!({"top":top*PT_PER_MM,"bottom":bottom*PT_PER_MM,"inside":inside*PT_PER_MM,"outside":outside*PT_PER_MM});
        }
        if let Some(bleed) = self.bleed_mm {
            params["bleed"] = json!(bleed * PT_PER_MM);
        }
        if let Some(columns) = self.columns {
            params["columns"] = json!(columns);
        }
        if let Some(gutter) = self.gutter_mm {
            params["gutter"] = json!(gutter * PT_PER_MM);
        }
        Ok(params)
    }

    /// Execute only immediately after creating a private document, never on the user's existing one.
    pub(crate) fn initial_actions(&self) -> Vec<EngineAction> {
        self.baseline_mm.map_or_else(Vec::new, |increment| {
            vec![EngineAction { command: "document.preferences".into(), params: json!({"baselineGrid":{"increment":increment*PT_PER_MM}}) }]
        })
    }

    /// Read-only compatibility check; callers must not silently reset document-wide settings.
    pub(crate) fn validate_existing(&self, state: &DocState) -> Result<(), String> {
        let near = |actual: f64, mm: f64| (actual - mm * PT_PER_MM).abs() < 0.1;
        for spread in state.doc.spreads.iter().chain(state.doc.parents.iter()) {
            for page in &spread.pages {
                if self.page_mm.is_some_and(|[w, h]| !near(page.width, w) || !near(page.height, h))
                    || self.margins_mm.is_some_and(|[t, b, i, o]| {
                        !near(page.margins.top, t) || !near(page.margins.bottom, b) || !near(page.margins.inside, i) || !near(page.margins.outside, o)
                    })
                    || self.columns.is_some_and(|n| page.columns.count != n)
                    || self.gutter_mm.is_some_and(|mm| !near(page.columns.gutter, mm))
                {
                    return Err(
                        "Requested print geometry differs from the existing document; create a new document or explicitly change its layout settings"
                            .into(),
                    );
                }
            }
        }
        if self.bleed_mm.is_some_and(|mm| state.doc.settings.bleed.iter().any(|&n| !near(n, mm)))
            || self.baseline_mm.is_some_and(|mm| !near(state.doc.settings.baseline_grid.increment, mm))
        {
            return Err("Requested bleed/baseline differs from the existing document; create a new document or explicitly change its settings".into());
        }
        Ok(())
    }

    pub(crate) fn min_image_ppi(&self) -> Option<f64> {
        self.min_ppi
    }

    /// Minimum native source pixels; resampling does not restore missing illustration detail.
    pub(crate) fn required_image_pixels(&self, rect: [f64; 4]) -> Result<Option<[u32; 2]>, String> {
        let Some(ppi) = self.min_ppi else { return Ok(None) };
        let [left, top, right, bottom] = rect;
        let dimensions = [(right - left) * ppi / 72.0, (bottom - top) * ppi / 72.0];
        if rect.iter().any(|n| !n.is_finite()) || dimensions.iter().any(|n| !n.is_finite() || *n <= 0.0 || *n > 32_768.0) {
            return Err("Requested illustration size exceeds the finite 1–32768 pixel print bounds".into());
        }
        let [width, height] = dimensions;
        Ok(Some([width.ceil() as u32, height.ceil() as u32]))
    }

    pub(crate) fn pdf_export_params(&self, path: Option<&str>) -> Result<Value, String> {
        self.validate_supported()?;
        let mut params = json!({"spreads":false,"bleed":self.bleed_mm.is_some_and(|mm| mm>0.0),"marks":false});
        if let Some(path) = path {
            params["path"] = json!(path);
        }
        if self.pdf_x4 {
            params["standard"] = json!("x4");
        }
        if let Some(offset) = self.crop_offset_mm {
            params["marks"] = json!({"crop":true,"offset":offset*PT_PER_MM});
        }
        Ok(params)
    }

    pub(crate) fn select_fonts(&self, state: &DocState) -> Result<PrintFonts, String> {
        let fonts = designcraft_fonts::FontDb::global().scoped(state.doc.font_scope);
        let mut notices = Vec::new();
        let mut select = |allowed: &[String], language: &str, probe: &str| -> Result<Option<AvailablePrintFont>, String> {
            if allowed.is_empty() {
                return Ok(None);
            }
            for (index, requested) in allowed.iter().enumerate() {
                // The production policy permits this explicitly reported installed-family fallback.
                // It is not a generic alias or a claim that the Pro face exists.
                let family = if !fonts.has_family(requested)
                    && requested.eq_ignore_ascii_case("Century Gothic Pro")
                    && fonts.has_family("Century Gothic")
                {
                    notices.push("Requested 'Century Gothic Pro' is unavailable; using installed 'Century Gothic' under the production fallback policy; the Pro font is not embedded".into());
                    "Century Gothic"
                } else {
                    requested.as_str()
                };
                if !fonts.has_family(family) {
                    continue;
                }
                let styles = fonts.styles(family);
                let regular = styles.iter().find(|s| s.eq_ignore_ascii_case("Regular")).or_else(|| styles.first());
                let Some(regular) = regular else { continue };
                let face = fonts.face(family, regular);
                if !probe.chars().all(|c| face.glyph_for(c) != 0) {
                    continue;
                }
                let heading = styles
                    .iter()
                    .find(|s| s.eq_ignore_ascii_case("Bold") && probe.chars().all(|c| fonts.face(family, s).glyph_for(c) != 0))
                    .unwrap_or(regular);
                if index > 0 {
                    notices.push(format!(
                        "Requested first-choice {language} font '{}' is unavailable/incomplete; using allowed alternative '{}'",
                        allowed.first().map_or("", String::as_str),
                        face.family
                    ));
                }
                if !regular.eq_ignore_ascii_case("Regular") || !heading.eq_ignore_ascii_case("Bold") {
                    notices.push(format!(
                        "{language} font '{}' uses available styles '{regular}' / '{heading}'; exact Regular/Bold pair unavailable",
                        face.family
                    ));
                }
                return Ok(Some(AvailablePrintFont { family: face.family.clone(), regular: regular.clone(), heading: heading.clone() }));
            }
            Err(format!(
                "No requested {language} font is available with the required glyphs; install or explicitly allow one of: {}",
                allowed.join(", ")
            ))
        };
        let english = select(&self.english_fonts, "English", "AaGg0123456789")?;
        let arabic = select(&self.arabic_fonts, "Arabic", "العربيةَُِّْ0123456789٠١٢٣٤٥٦٧٨٩")?;
        Ok(PrintFonts { english, arabic, notices })
    }
}

fn clean(value: &str) -> String {
    value.chars().filter(|c| !matches!(c, '*' | '`')).collect::<String>().trim().to_string()
}

fn specification(line: &str) -> Option<(String, &str)> {
    let line = line.trim();
    if line.starts_with('|') {
        let mut columns = line.split('|').skip(1);
        let key = clean(columns.next()?).to_lowercase();
        let value = columns.next()?;
        return Some((key, value));
    }
    let (label, value) = line.split_once(':')?;
    let key = clean(label).trim_start_matches(['*', '-', ' ']).to_lowercase();
    Some((key, value))
}

fn recognized_spec(key: &str) -> bool {
    matches!(
        key,
        "trim size"
            | "page size"
            | "bleed"
            | "safe margin / live area"
            | "safe margins"
            | "margins"
            | "grid system"
            | "english primary font"
            | "arabic primary font"
            | "color profile"
            | "colour profile"
            | "image resolution"
            | "effective image resolution"
            | "pdf standard"
            | "export standard"
            | "prepress preflight export"
    )
}

fn put_fonts(target: &mut Vec<String>, value: Vec<String>, label: &str) -> Result<(), String> {
    if !target.is_empty() && *target != value {
        return Err(format!("Print specification has conflicting {label} values"));
    }
    *target = value;
    Ok(())
}

fn put<T: PartialEq>(target: &mut Option<T>, value: T, label: &str) -> Result<(), String> {
    if target.as_ref().is_some_and(|old| *old != value) {
        return Err(format!("Print specification has conflicting {label} values"));
    }
    *target = Some(value);
    Ok(())
}

fn bounded(value: f64, min: f64, max: f64, label: &str) -> Result<f64, String> {
    if !value.is_finite() || !(min..=max).contains(&value) {
        return Err(format!("Print {label} must be finite and between {min} and {max}"));
    }
    Ok(value)
}

fn require_mm(value: &str, label: &str) -> Result<(), String> {
    if !value.to_lowercase().contains("mm") {
        return Err(format!("Print {label} requires explicit millimeters (mm)"));
    }
    Ok(())
}

fn numbers(value: &str) -> Result<Vec<f64>, String> {
    let mut out = Vec::new();
    let mut token = String::new();
    let mut chars = value.chars().chain(std::iter::once(' ')).peekable();
    while let Some(c) = chars.next() {
        if token.is_empty() {
            if c.is_ascii_digit() || (matches!(c, '.' | '-' | '+') && chars.peek().is_some_and(char::is_ascii_digit)) {
                token.push(c);
            }
            continue;
        }
        if c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E') || (matches!(c, '+' | '-') && token.ends_with(['e', 'E'])) {
            token.push(c);
            continue;
        }
        out.push(token.parse::<f64>().map_err(|_| "Print specification contains an invalid numeric value")?);
        if out.len() > 32 {
            return Err("Print specification has too many numeric values".into());
        }
        token.clear();
        if matches!(c, '-' | '+') && chars.peek().is_some_and(char::is_ascii_digit) {
            token.push(c);
        }
    }
    Ok(out)
}

fn first_number(value: &str, label: &str) -> Result<f64, String> {
    numbers(value)?.first().copied().ok_or_else(|| format!("Print {label} needs a numeric value"))
}

fn preceding_number(value: &str, end: usize, label: &str) -> Result<f64, String> {
    numbers(value.get(..end).ok_or("Invalid print number boundary")?)?.last().copied().ok_or_else(|| format!("Print {label} needs a numeric value"))
}

fn font_candidates(value: &str) -> Result<Vec<String>, String> {
    let names = value.split('(').next().unwrap_or(value).split(" with ").next().unwrap_or(value);
    let names = names.replace(" or ", "/");
    let out: Vec<String> = names.split(['/', ',']).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect();
    if out.is_empty() || out.len() > 8 || out.iter().any(|s| s.len() > 160) {
        return Err("Requested print font list must have 1–8 bounded family names".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use designcraft_engine::Session;

    const PRINT: &str = "## GLOBAL PRINT SPECIFICATIONS\n| Trim Size | A4 Portrait (210 mm × 297 mm) |\n| Bleed | 3 mm on all four edges |\n| Safe Margin / Live Area | 15 mm top, bottom, and outside margins; 18 mm inside gutter margin |\n| Grid System | 6-Column Modular Grid (Gutter: 4.5 mm) with 8 mm baseline grid |\n### PAGE 01 — Cover\nArt has 72 DPI.\n4. **Prepress Preflight Export:** Export as **PDF/X-4:2010** with `3 mm` bleed on all sides, crop marks offset by `4 mm`, all fonts 100% embedded, and effective image resolution ≥ `300 PPI`.";

    #[test]
    fn explicit_global_contract_drives_native_geometry_and_export() {
        let contract = PrintContract::from_prompt(PRINT).unwrap().unwrap();
        contract.validate_supported().unwrap();
        assert_eq!(contract.margins_mm, Some([15.0, 15.0, 18.0, 15.0]));
        assert_eq!(contract.min_image_ppi(), Some(300.0));
        let params = contract.document_params(None, 2).unwrap();
        let mut session = Session::new();
        session.execute("file.new", &params).unwrap();
        for action in contract.initial_actions() {
            session.execute(&action.command, &action.params).unwrap();
        }
        let doc = session.doc().unwrap();
        contract.validate_existing(doc).unwrap();
        let page = doc.doc.page(0).unwrap();
        assert_eq!(page.columns.count, 6);
        assert!((page.columns.gutter / PT_PER_MM - 4.5).abs() < 1e-6);
        assert!((doc.doc.settings.baseline_grid.increment / PT_PER_MM - 8.0).abs() < 1e-6);
        let export = contract.pdf_export_params(Some("proof.pdf")).unwrap();
        assert_eq!(export["standard"], "x4");
        assert_eq!(export["bleed"], true);
        assert_eq!(export["marks"]["crop"], true);
        assert!((export["marks"]["offset"].as_f64().unwrap() - 4.0 * PT_PER_MM).abs() < 1e-6);
        assert_eq!(contract.required_image_pixels([0.0, 0.0, 144.0, 72.0]).unwrap(), Some([600, 300]));
    }

    #[test]
    fn no_explicit_global_spec_preserves_existing_behavior() {
        assert!(PrintContract::from_prompt("Draw an A4-looking poster.\n### PAGE 1\nMake the photo 300 DPI.").unwrap().is_none());
        let contract = PrintContract::default();
        assert_eq!(contract.document_params(None, 3).unwrap(), crate::design::document_params(None, 3));
        assert!(contract.initial_actions().is_empty());
        assert_eq!(contract.pdf_export_params(None).unwrap(), json!({"spreads":false,"bleed":false,"marks":false}));
    }

    #[test]
    fn existing_document_contract_is_read_only_and_rejects_mismatch() {
        let contract = PrintContract::from_prompt(PRINT).unwrap().unwrap();
        let mut session = Session::new();
        session.execute("file.new", &crate::design::document_params(None, 2)).unwrap();
        let before = session.doc().unwrap().clone();
        assert!(contract.validate_existing(session.doc().unwrap()).unwrap_err().contains("existing document"));
        assert_eq!(before.revision, session.doc().unwrap().revision);
        assert_eq!(before.doc.settings, session.doc().unwrap().doc.settings);
    }

    #[test]
    fn contradictory_and_unsupported_specs_are_visible_and_block_final_print_claims() {
        let conflicting = PRINT.replace("(Gutter:", "(Column width: 25.5 mm, Gutter:");
        let conflicting = PrintContract::from_prompt(&conflicting).unwrap().unwrap();
        conflicting.validate_supported().unwrap();
        assert!(conflicting.validate_final_export().unwrap_err().contains("column width"));
        let unsupported = PRINT.replace("| Bleed", "| Color Profile | CMYK (ISO Coated v2 for Cover; PSO Uncoated for Interior) |\n| Bleed");
        let unsupported = PrintContract::from_prompt(&unsupported).unwrap().unwrap();
        unsupported.validate_supported().unwrap();
        assert!(unsupported.validate_final_export().unwrap_err().contains("ICC"));
        assert!(PrintContract::from_prompt(&PRINT.replace("3 mm on all", "-3 mm on all")).is_err());
        assert!(PrintContract::from_prompt(&PRINT.replace("6-Column", "999999999-Column")).is_err());
        assert!(PrintContract::from_prompt(&PRINT.replace("300 PPI", "0 PPI")).is_err());
        assert!(PrintContract::from_prompt(&PRINT.replace("3 mm on all", "1e309 mm on all")).is_err());
    }

    #[test]
    fn duplicate_specs_and_hostile_inputs_do_not_silently_override_requirements() {
        let duplicate = PRINT.replace("| Bleed |", "| Bleed | 5 mm all edges |\n| Bleed |");
        assert!(PrintContract::from_prompt(&duplicate).unwrap_err().contains("conflicting"));
        assert!(PrintContract::from_prompt(&"x".repeat(MAX_SCRIPT_BYTES + 1)).is_err());
        let contract = PrintContract::from_prompt(PRINT).unwrap().unwrap();
        assert!(contract.required_image_pixels([0.0, 0.0, f64::INFINITY, 72.0]).is_err());
        assert!(contract.required_image_pixels([0.0, 0.0, 1e100, 72.0]).is_err());
        assert!(contract.required_image_pixels([0.0, 0.0, 0.0, 72.0]).is_err());
    }

    #[test]
    fn font_selection_never_substitutes_an_unrequested_family() {
        let contract = PrintContract::from_prompt("* **English Primary Font:** *Missing Family 1* or *Missing Family 2* (Headings 22 pt).\n* **Arabic Primary Font:** *Missing Arabic Family* with full diacritics.").unwrap().unwrap();
        assert_eq!(contract.english_fonts, ["Missing Family 1", "Missing Family 2"]);
        assert_eq!(contract.arabic_fonts, ["Missing Arabic Family"]);
        let mut session = Session::new();
        session.execute("file.new", &json!({})).unwrap();
        let error = contract.select_fonts(session.doc().unwrap()).unwrap_err();
        assert!(error.contains("requested English font"));
        assert!(!error.contains("Tahoma"));
    }

    #[test]
    fn production_script_comma_separated_font_alternatives_are_kept_as_families() {
        let source = format!("{PRINT}\n");
        let source = source.replace("### PAGE 01", "* **English Primary Font:** *Andika* or *Century Gothic Pro* / *Fredoka* (Headings: 22–28 pt Bold).\n* **Arabic Primary Font:** *Readex Pro*, *Cairo*, or *Tajawal* with full diacritics/tashkeel on key vocabulary.\n### PAGE 01");
        let contract = PrintContract::from_prompt(&source).unwrap().unwrap();
        assert_eq!(contract.english_fonts, ["Andika", "Century Gothic Pro", "Fredoka"]);
        assert_eq!(contract.arabic_fonts, ["Readex Pro", "Cairo", "Tajawal"]);
        assert_eq!(contract.min_image_ppi(), Some(300.0));
    }
}
