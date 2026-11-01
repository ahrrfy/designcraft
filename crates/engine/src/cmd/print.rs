//! File › Print: the pages go to a printer through the system print spooler (craft-print), as PDF.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use super::{CommandSpec, bad, cmd, has_doc};
use crate::{Result, Session};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "file.preparePrint","Prepare Frozen Print",[],None,
            "{printer?, copies?: 1, pages?: string | [n], spreads?: bool, marks?: bool | object, bleed?: bool, standard?: none|x4|x1a|a2b, scaling?: exact|shrinkToFit, duplex?: off|longEdge|shortEdge, grayscale?: bool, minPpi?: 150, created?: int, …the PDF settings of file.exportPdf} → {token, sha256, bytes, pages, manifest}; freezes one preflighted PDF with printer settings, replacing the previous artifact",has_doc,prepare_command),
        cmd!(noundo "file.printPrepared","Print Frozen PDF",[],None,
            "{token, dryRun?: bool} → submission evidence for the exact engine-owned PDF and frozen printer settings; no document re-export",super::always,print_prepared_command),
        cmd!(query "file.savePreparedPrint","Save Frozen Print PDF",[],None,
            "{token, path} → {path, sha256, bytes, manifest}; writes the exact frozen PDF with .pdf extension",super::always,save_prepared_command),
        cmd!(query "file.printers", "Printers", [], None, "{} → {printers: [names], default}", super::always, |_, _| Ok(printers())),
        cmd!(
            noundo "file.print",
            "Print…",
            ["File"],
            Some("Cmd+P"),
            "{printer?, copies?: 1, pages?: \"1-3,5\", spreads?, marks?, bleed?, standard?: none|x4|x1a|a2b, minPpi?: 150, scaling?: exact|shrinkToFit (default exact: preserve physical dimensions), duplex?: off|longEdge|shortEdge (default off), grayscale?: bool, dryRun?: bool (prepare and preview submission without printing), created?: int, …the PDF settings of file.exportPdf} → {printer, copies, pages, scaling, duplex, grayscale, command, bytes, warnings, token, sha256, manifest}",
            has_doc,
            print
        ),
    ]
}

fn printers() -> Value {
    let printers = craft_print::printers();
    let default = printers.iter().find(|p| p.default).map(|p| p.name.clone());
    let names: Vec<_> = printers.into_iter().map(|p| p.name).collect();
    json!({"printers": names, "default": default})
}

fn print(s: &mut Session, p: &Value) -> Result<Value> {
    #[cfg(target_arch = "wasm32")]
    {
        super::export::options(p, s.doc()?.doc.page_count())?;
        Err(bad("file.print", "printing isn't available on the web: export a PDF and print it"))
    }
    #[cfg(not(target_arch = "wasm32"))]
    print_with(s, p, craft_print::submit)
}

#[cfg(not(target_arch = "wasm32"))]
fn print_with(
    s: &mut Session,
    p: &Value,
    submit: impl FnOnce(&[u8], &craft_print::Job) -> std::result::Result<String, craft_print::Error>,
) -> Result<Value> {
    let prepared = prepare(s, p)?;
    let dry = bool_option(p, "dryRun")?;
    let prepared = Arc::new(prepared);
    s.prepared_print = Some(prepared.clone());
    submit_with(&prepared, dry, submit)
}

/// Immutable print artifact: only the engine can construct its bytes, manifest and printer job.
/// A caller may inspect copies of the manifest or borrow the bytes; neither is mutable.
pub struct PreparedPrint {
    bytes: Arc<[u8]>,
    sha256: String,
    token: String,
    manifest: Value,
    job: craft_print::Job,
    pages: usize,
}
impl PreparedPrint {
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
    pub fn token(&self) -> &str {
        &self.token
    }
    pub fn manifest(&self) -> Value {
        self.manifest.clone()
    }
    pub fn evidence(&self) -> Value {
        json!({"token":self.token,"sha256":self.sha256,"bytes":self.bytes.len(),"pages":self.pages,"manifest":self.manifest})
    }
    fn verify(&self) -> Result<()> {
        if digest(&self.bytes) != self.sha256
            || digest(&serde_json::to_vec(&self.manifest).map_err(|e| bad("file.printPrepared", e.to_string()))?) != self.token
            || self.manifest.get("job") != Some(&job_evidence(&self.job))
            || self.manifest.get("pdf").and_then(|pdf| pdf.get("sha256")).and_then(Value::as_str) != Some(self.sha256.as_str())
        {
            return Err(bad("file.printPrepared", "prepared PDF integrity check failed"));
        }
        Ok(())
    }
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn bool_option(p: &Value, key: &str) -> Result<bool> {
    match p.get(key) {
        None => Ok(false),
        Some(v) => v.as_bool().ok_or_else(|| bad("file.print", format!("{key} must be a boolean"))),
    }
}

/// Build and preflight the complete PDF once. The returned object binds the exact PDF bytes,
/// document identity, font/color evidence, and all printer options into one immutable token.
pub fn prepare(s: &Session, p: &Value) -> Result<PreparedPrint> {
    prepare_with_printers(s, p, craft_print::printers)
}

fn prepare_with_printers(s: &Session, p: &Value, printers: impl FnOnce() -> Vec<craft_print::Printer>) -> Result<PreparedPrint> {
    let allowed = [
        "printer",
        "copies",
        "pages",
        "spreads",
        "marks",
        "bleed",
        "standard",
        "minPpi",
        "scaling",
        "duplex",
        "grayscale",
        "dryRun",
        "created",
        "compressImages",
        "title",
        "author",
        "tagged",
        "media",
        "flatten",
        "advanceSeconds",
        "fullScreen",
        "bookmarksPanel",
        "pageLayout",
        "view",
    ];
    let object = p.as_object().ok_or_else(|| bad("file.preparePrint", "parameters must be an object"))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(bad("file.preparePrint", "unsupported preparation parameter"));
    }
    let d = &s.doc()?.doc;
    if s.doc()?.interaction.is_some() || s.doc()?.preview_stash.is_some() || s.tool.composing() {
        return Err(bad("file.preparePrint", "finish the active interaction or preview before freezing print output"));
    }
    if d.page_count() > 1000 {
        return Err(bad("file.preparePrint", "print preparation supports at most 1000 pages"));
    }
    let asset_bytes = d
        .assets
        .values()
        .try_fold(0usize, |sum, asset| sum.checked_add(asset.data.len()))
        .ok_or_else(|| bad("file.preparePrint", "asset size overflow"))?;
    if asset_bytes > 128 * 1024 * 1024 {
        return Err(bad("file.preparePrint", "print assets exceed the 128-MiB preparation limit"));
    }
    let min_ppi = match p.get("minPpi") {
        None => 150.0,
        Some(v) => v
            .as_f64()
            .filter(|v| v.is_finite() && (1.0..=2400.0).contains(v))
            .ok_or_else(|| bad("file.preparePrint", "minPpi must be finite and between 1 and 2400"))?,
    };
    for key in ["spreads", "bleed", "grayscale", "dryRun"] {
        if p.get(key).is_some() {
            bool_option(p, key)?;
        }
    }
    if p.get("standard").is_some_and(|v| !v.is_string()) {
        return Err(bad("file.preparePrint", "standard must be a string"));
    }
    if let Some(marks) = p.get("marks") {
        match marks {
            Value::Bool(_) => {}
            Value::Object(values) => {
                for (key, value) in values {
                    let valid = match key.as_str() {
                        "crop" | "bleed" | "pageInfo" => value.is_boolean(),
                        "weight" => value.as_f64().is_some_and(|n| n.is_finite() && (0.01..=10.0).contains(&n)),
                        "offset" => value.as_f64().is_some_and(|n| n.is_finite() && (0.0..=1000.0).contains(&n)),
                        _ => false,
                    };
                    if !valid {
                        return Err(bad("file.preparePrint", format!("invalid printer marks setting `{key}`")));
                    }
                }
            }
            _ => return Err(bad("file.preparePrint", "marks must be a boolean or printer-marks object")),
        }
    }
    let opts = super::export::options(p, d.page_count())?;
    let copies = match p.get("copies") {
        None => 1,
        Some(v) => {
            v.as_u64().filter(|n| (1..=999).contains(n)).ok_or_else(|| bad("file.print", "copies must be a whole number from 1 to 999"))? as u32
        }
    };
    let scaling = match p.get("scaling") {
        None => craft_print::Scaling::Exact,
        Some(Value::String(v)) if v == "exact" => craft_print::Scaling::Exact,
        Some(Value::String(v)) if v == "shrinkToFit" => craft_print::Scaling::ShrinkToFit,
        _ => return Err(bad("file.print", "scaling must be exact or shrinkToFit")),
    };
    let duplex = match p.get("duplex") {
        None => craft_print::Duplex::Off,
        Some(Value::String(v)) if v == "off" => craft_print::Duplex::Off,
        Some(Value::String(v)) if v == "longEdge" => craft_print::Duplex::LongEdge,
        Some(Value::String(v)) if v == "shortEdge" => craft_print::Duplex::ShortEdge,
        _ => return Err(bad("file.print", "duplex must be off, longEdge or shortEdge")),
    };
    let grayscale = bool_option(p, "grayscale")?;
    if p.get("printer").is_some_and(|v| !v.as_str().is_some_and(|name| !name.is_empty() && name.len() <= 256 && !name.chars().any(char::is_control)))
    {
        return Err(bad("file.print", "printer must contain 1 to 256 bytes"));
    }
    let job = craft_print::Job {
        // A null destination would let the OS select a different default after approval.
        // Freeze the resolved queue name, or require an explicit choice when unavailable.
        printer: Some(
            p.get("printer")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| printers().into_iter().find(|printer| printer.default).map(|printer| printer.name))
                .ok_or_else(|| bad("file.preparePrint", "no default printer is available; select an explicit printer before preparing output"))?,
        ),
        copies,
        scaling,
        duplex: Some(duplex),
        grayscale,
        title: d.title.clone(),
        ..craft_print::Job::default()
    };
    let issues = super::preflight::check(s, min_ppi);
    let errors: Vec<_> = issues.iter().filter(|issue| issue.severity == "error").collect();
    if !errors.is_empty() {
        return Err(bad(
            "file.preparePrint",
            format!("preflight rejected print PDF: {}", serde_json::to_string(&errors).map_err(|e| bad("file.preparePrint", e.to_string()))?),
        ));
    }
    let color = designcraft_color::cms::active_settings();
    let color_state =
        json!({"rgb":color.rgb,"cmyk":color.cmyk,"intent":color.intent.id(),"blackPointCompensation":color.bpc,"blendSpace":d.settings.blend_space});
    let fonts = serde_json::to_value(super::fonts::used_fonts(d)).map_err(|e| bad("file.preparePrint", e.to_string()))?;
    let pdf = super::export::pdf_export(s, p)?;
    if pdf.bytes.len() > 128 * 1024 * 1024 {
        return Err(bad("file.preparePrint", "prepared PDF exceeds 128 MiB"));
    }
    let pages = designcraft_render::pdf_page_count(&pdf.bytes).ok_or_else(|| bad("file.preparePrint", "exported PDF cannot be parsed"))?;
    if pages != pdf.pages {
        return Err(bad("file.preparePrint", "exported PDF page count does not match the export report"));
    }
    let mut geometry = Vec::with_capacity(pages);
    for page in 0..pages {
        let (width, height) = designcraft_render::pdf_page_size(&pdf.bytes, page)
            .ok_or_else(|| bad("file.preparePrint", "exported PDF page geometry cannot be inspected"))?;
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return Err(bad("file.preparePrint", "exported PDF has invalid page dimensions"));
        }
        geometry.push(json!({"page":page+1,"widthPt":width,"heightPt":height}));
    }
    let color_after = designcraft_color::cms::active_settings();
    if color_after.rgb != color.rgb || color_after.cmyk != color.cmyk || color_after.intent != color.intent || color_after.bpc != color.bpc {
        return Err(bad("file.preparePrint", "color settings changed while preparing print output; prepare again"));
    }
    if fonts != serde_json::to_value(super::fonts::used_fonts(d)).map_err(|e| bad("file.preparePrint", e.to_string()))? {
        return Err(bad("file.preparePrint", "font resolution changed while preparing print output; prepare again"));
    }
    let sha256 = digest(&pdf.bytes);
    let manifest = json!({"schemaVersion":1,"format":"pdf","documentUid":s.doc()?.uid,"documentRevision":s.doc()?.revision,"title":d.title,
        "pdf":{"sha256":sha256,"bytes":pdf.bytes.len(),"pages":pages,"geometry":geometry,"standard":p.get("standard").cloned().unwrap_or(json!("none")),"exportWarnings":pdf.warnings},
        "source":{"pages":opts.pages,"spreads":opts.spreads,"bleed":opts.bleed,"marks":{"crop":opts.marks.crop,"bleed":opts.marks.bleed,"pageInfo":opts.marks.page_info,"weight":opts.marks.weight,"offset":opts.marks.offset}},
        "preflight":{"errors":0,"minPpi":min_ppi,"issues":issues},"fonts":fonts,"color":color_state,
        "job":job_evidence(&job),"guarantee":"The same prepared PDF bytes are handed to the software spooler; physical output, printer/RIP scaling and color accuracy require verification"});
    let token = digest(&serde_json::to_vec(&manifest).map_err(|e| bad("file.preparePrint", e.to_string()))?);
    Ok(PreparedPrint { bytes: pdf.bytes.into(), sha256, token, manifest, job, pages })
}

fn job_evidence(job: &craft_print::Job) -> Value {
    let scaling_name = match job.scaling {
        craft_print::Scaling::Exact => "exact",
        craft_print::Scaling::ShrinkToFit => "shrinkToFit",
    };
    let duplex_name = match job.duplex {
        Some(craft_print::Duplex::LongEdge) => "longEdge",
        Some(craft_print::Duplex::ShortEdge) => "shortEdge",
        _ => "off",
    };
    json!({"printer":job.printer,"copies":job.copies,"scaling":scaling_name,"duplex":duplex_name,"grayscale":job.grayscale,"collate":job.collate,"title":job.title,"options":job.options})
}

fn prepare_command(s: &mut Session, p: &Value) -> Result<Value> {
    let prepared = Arc::new(prepare(s, p)?);
    let evidence = prepared.evidence();
    s.prepared_print = Some(prepared);
    Ok(evidence)
}
fn selected(s: &Session, p: &Value, allowed: &[&str]) -> Result<Arc<PreparedPrint>> {
    let object = p.as_object().ok_or_else(|| bad("file.printPrepared", "parameters must be an object"))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(bad("file.printPrepared", "printer settings and PDF bytes are frozen; prepare a new artifact to change them"));
    }
    let token = p.get("token").and_then(Value::as_str).ok_or_else(|| bad("file.printPrepared", "token required"))?;
    let artifact = s
        .prepared_print
        .as_ref()
        .filter(|artifact| artifact.token == token)
        .ok_or_else(|| bad("file.printPrepared", "unknown or replaced prepared print token"))?
        .clone();
    artifact.verify()?;
    Ok(artifact)
}
fn save_prepared_command(s: &mut Session, p: &Value) -> Result<Value> {
    let artifact = selected(s, p, &["token", "path"])?;
    let path = p.get("path").and_then(Value::as_str).ok_or_else(|| bad("file.savePreparedPrint", "path required"))?;
    let path = super::formats::checked_path("file.savePreparedPrint", path, "pdf")?;
    #[cfg(not(target_arch = "wasm32"))]
    crate::io::write_file(&path, artifact.bytes()).map_err(crate::EngineError::Other)?;
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (&artifact, &path);
        return Err(bad("file.savePreparedPrint", "file writing is unavailable on the web"));
    }
    #[cfg(not(target_arch = "wasm32"))]
    Ok(json!({"path":path,"bytes":artifact.bytes.len(),"sha256":artifact.sha256,"manifest":artifact.manifest}))
}
fn print_prepared_command(s: &mut Session, p: &Value) -> Result<Value> {
    let artifact = selected(s, p, &["token", "dryRun"])?;
    let dry = bool_option(p, "dryRun")?;
    #[cfg(not(target_arch = "wasm32"))]
    {
        submit_with(&artifact, dry, craft_print::submit)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (artifact, dry);
        Err(bad("file.printPrepared", "printing is unavailable on the web"))
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn submit_with(
    prepared: &PreparedPrint,
    dry: bool,
    submit: impl FnOnce(&[u8], &craft_print::Job) -> std::result::Result<String, craft_print::Error>,
) -> Result<Value> {
    prepared.verify()?;
    let job = &prepared.job;
    let command = craft_print::command_preview(job).map_err(|e| bad("file.printPrepared", e.to_string()))?;
    let scaling_name = match job.scaling {
        craft_print::Scaling::Exact => "exact",
        craft_print::Scaling::ShrinkToFit => "shrinkToFit",
    };
    let duplex_name = match job.duplex.unwrap_or(craft_print::Duplex::Off) {
        craft_print::Duplex::Off => "off",
        craft_print::Duplex::LongEdge => "longEdge",
        craft_print::Duplex::ShortEdge => "shortEdge",
    };
    let warnings = prepared.manifest.get("pdf").and_then(|pdf| pdf.get("exportWarnings")).cloned().unwrap_or_else(|| json!([]));
    let out = json!({"printer": job.printer, "copies": job.copies, "pages": prepared.pages, "scaling": scaling_name, "duplex": duplex_name, "grayscale": job.grayscale, "dryRun": dry, "command": command, "bytes": prepared.bytes.len(), "warnings": warnings, "token": prepared.token, "sha256": prepared.sha256, "manifest": prepared.manifest});
    if !dry {
        submit(&prepared.bytes, job).map_err(|e| bad("file.printPrepared", e.to_string()))?;
    }
    Ok(out)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn print_builds_the_spool_command() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"pages": 4})).unwrap();
        let r = s.execute("file.print", &json!({"printer": "Office", "copies": 2, "pages": "2-3", "dryRun": true})).unwrap();
        assert_eq!(r["pages"], 2);
        let command: Vec<&str> = r["command"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        if craft_print::backend() == Some(craft_print::Backend::Cups) {
            assert_eq!(&command[..5], ["lp", "-d", "Office", "-n", "2"]);
            assert!(command.contains(&"fit-to-page=false"));
            assert!(command.contains(&"sides=one-sided"));
        } else {
            assert_eq!(&command[..4], ["xps-print", "printer=Office", "copies=2", "collate=true"]);
            assert!(command.iter().any(|arg| arg.starts_with("duplex=")), "{command:?}");
            assert!(command.iter().any(|arg| arg.starts_with("scaling=")), "{command:?}");
        }
        assert!(r["bytes"].as_u64().unwrap() > 500);
        assert!(s.execute("file.print", &json!({"pages": "9", "dryRun": true})).is_err());
        assert!(s.execute("file.printers", &json!({})).unwrap()["printers"].is_array());
    }

    #[test]
    fn print_submits_the_selected_export_and_reports_refusal() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"pages": 4})).unwrap();
        // A fixed creation date: both exports would otherwise take the clock's second.
        let params = json!({"printer": "Office (east)", "copies": 2, "pages": "2-3", "marks": true, "bleed": true, "created": 1_700_000_000});
        let expected = super::super::export::pdf_export(&s, &params).unwrap().bytes;
        let r = print_with(&mut s, &params, |pdf, job| {
            // The production export options and full bytes reach the shared spooler boundary.
            assert_eq!(pdf, expected);
            assert!(pdf.starts_with(b"%PDF-"));
            assert_eq!(job.printer.as_deref(), Some("Office (east)"));
            assert_eq!(job.copies, 2);
            assert_eq!(job.duplex, Some(craft_print::Duplex::Off));
            assert!(!job.grayscale);
            assert_eq!(job.scaling, craft_print::Scaling::Exact);
            assert!(!job.title.is_empty());
            Ok("request id is Office-1".into())
        })
        .unwrap();
        assert_eq!(r["pages"], 2);
        let refusal = print_with(&mut s, &params, |_, _| Err(craft_print::Error::Spool("printer offline".into()))).unwrap_err();
        assert!(refusal.to_string().contains("printer offline"));
        print_with(&mut s, &json!({"printer":"Office","dryRun": true}), |_, _| panic!("dry runs must not spool")).unwrap();
        assert!(print_with(&mut s, &json!({"pages": "9"}), |_, _| panic!("invalid ranges must not spool")).is_err());
        assert!(print_with(&mut s, &json!({"created": "today"}), |_, _| panic!("an invalid date must not spool")).is_err());
    }

    #[test]
    fn print_options_reach_spooler_and_dry_run_evidence_without_submission() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let params = json!({"printer":"Office","scaling":"shrinkToFit","duplex":"shortEdge","grayscale":true});
        let r = print_with(&mut s, &params, |_, job| {
            assert_eq!(job.scaling, craft_print::Scaling::ShrinkToFit);
            assert_eq!(job.duplex, Some(craft_print::Duplex::ShortEdge));
            assert!(job.grayscale);
            Ok("test submission".into())
        })
        .unwrap();
        assert_eq!(r["scaling"], "shrinkToFit");
        assert_eq!(r["duplex"], "shortEdge");
        assert_eq!(r["grayscale"], true);
        for invalid in [
            json!({"copies":0}),
            json!({"copies":"2"}),
            json!({"scaling":"fit"}),
            json!({"duplex":"yes"}),
            json!({"grayscale":1}),
            json!({"dryRun":"true"}),
        ] {
            assert!(print_with(&mut s, &invalid, |_, _| panic!("invalid options must never spool")).is_err());
        }
    }

    #[test]
    fn frozen_pdf_spools_saved_bytes_and_job_even_after_document_changes() {
        let directory = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.execute("file.new", &json!({"preset":"A4","pages":2,"facingPages":false})).unwrap();
        let before = s.doc().unwrap().clone();
        let proof =
            s.execute("file.preparePrint", &json!({"printer":"Reviewed queue","copies":2,"duplex":"longEdge","grayscale":true,"pages":"2"})).unwrap();
        let artifact = s.prepared_print.as_ref().unwrap().clone();
        let pdf_path = directory.path().join("approved.pdf");
        let saved = s.execute("file.savePreparedPrint", &json!({"token":proof["token"],"path":pdf_path})).unwrap();
        let frozen = std::fs::read(&pdf_path).unwrap();
        assert_eq!(digest(&frozen), proof["sha256"].as_str().unwrap());
        assert_eq!(saved["sha256"], proof["sha256"]);
        assert_eq!(proof["manifest"]["documentRevision"], before.revision);
        assert_eq!(proof["manifest"]["preflight"]["errors"], 0);
        assert!(proof["manifest"]["fonts"].is_array());
        assert!(proof["manifest"]["color"]["rgb"].is_string());
        assert_eq!(designcraft_render::pdf_page_count(&frozen), Some(1));
        s.execute("layout.documentSetup", &json!({"width":300,"height":400})).unwrap();
        assert_ne!(s.doc().unwrap().revision, before.revision);
        submit_with(&artifact, false, |bytes, job| {
            assert_eq!(bytes, frozen);
            assert_eq!(job.printer.as_deref(), Some("Reviewed queue"));
            assert_eq!(job.copies, 2);
            assert_eq!(job.duplex, Some(craft_print::Duplex::LongEdge));
            assert_eq!(job.scaling, craft_print::Scaling::Exact);
            assert!(job.grayscale);
            Ok("mock accepted".into())
        })
        .unwrap();
        let evidence = s.execute("file.printPrepared", &json!({"token":proof["token"],"dryRun":true})).unwrap();
        assert_eq!(evidence["sha256"], proof["sha256"]);
        assert_eq!(evidence["manifest"], proof["manifest"]);
    }

    #[test]
    fn frozen_artifacts_reject_option_changes_replacement_forgery_and_tampering() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let first = s.execute("file.preparePrint", &json!({"printer":"Queue A"})).unwrap();
        assert!(s.execute("file.printPrepared", &json!({"token":first["token"],"printer":"Queue B","dryRun":true})).is_err());
        assert!(s.execute("file.printPrepared", &json!({"token":"forged","dryRun":true})).is_err());
        let second = s.execute("file.preparePrint", &json!({"printer":"Queue B"})).unwrap();
        assert_ne!(first["token"], second["token"]);
        assert!(s.execute("file.printPrepared", &json!({"token":first["token"],"dryRun":true})).is_err());
        let mut corrupted = prepare(&s, &json!({"printer":"Queue B"})).unwrap();
        corrupted.bytes = Arc::from(b"%PDF-tampered".as_slice());
        assert!(submit_with(&corrupted, false, |_, _| panic!("tampered bytes must not reach spooler")).is_err());
        let mut wrong_job = prepare(&s, &json!({"printer":"Queue B"})).unwrap();
        wrong_job.job.copies = 999;
        assert!(submit_with(&wrong_job, false, |_, _| panic!("unbound job must not reach spooler")).is_err());
    }

    #[test]
    fn print_preparation_blocks_preflight_errors_and_invalid_options_without_replacing_proof() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let proof = s.execute("file.preparePrint", &json!({"printer":"Queue A"})).unwrap();
        for params in [
            json!({"printer":"Queue A","marks":3}),
            json!({"printer":"Queue A","marks":{"crop":"yes"}}),
            json!({"printer":"Queue A","minPpi":0}),
            json!({"printer":"Queue A","spreads":"false"}),
        ] {
            assert!(s.execute("file.preparePrint", &params).is_err());
            assert_eq!(s.prepared_print.as_ref().unwrap().token(), proof["token"].as_str().unwrap());
        }
        s.execute("frame.create", &json!({"rect":[36,36,40,40],"content":"text","text":"overset ".repeat(100)})).unwrap();
        let err = s.execute("file.preparePrint", &json!({"printer":"Queue A"})).unwrap_err();
        assert!(err.to_string().contains("overset"), "{err}");
        assert_eq!(s.prepared_print.as_ref().unwrap().token(), proof["token"].as_str().unwrap());
        s.execute("edit.undo", &json!({})).unwrap();
    }

    #[test]
    fn default_queue_is_resolved_during_preparation_and_never_during_submission() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let prepared = prepare_with_printers(&s, &json!({}), || {
            vec![craft_print::Printer { name: "Original default".into(), description: String::new(), default: true, two_sided: None }]
        })
        .unwrap();
        assert_eq!(prepared.manifest["job"]["printer"], "Original default");
        // The submit boundary receives the stored explicit queue, regardless of what a later
        // operating-system default would be; it has no access to a printer-discovery callback.
        submit_with(&prepared, false, |_, job| {
            assert_eq!(job.printer.as_deref(), Some("Original default"));
            Ok("mock accepted".into())
        })
        .unwrap();
        assert!(prepare_with_printers(&s, &json!({}), Vec::new).is_err());
        prepare_with_printers(&s, &json!({"printer":"Explicit queue"}), || panic!("an explicit queue needs no discovery")).unwrap();
    }

    /// File › Print spools the PDF that File › Export PDF writes with the same settings: soft
    /// effects as images, CMYK print color, the same warnings.
    #[test]
    fn print_spools_what_export_pdf_writes() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let id = s.execute("frame.create", &json!({"rect": [72, 72, 200, 200]})).unwrap()["id"].clone();
        s.execute("object.fill", &json!({"ids": [id], "swatch": "C=100 M=0 Y=0 K=0"})).unwrap();
        s.execute("object.dropShadow", &json!({"ids": [id], "on": true, "distance": 8, "size": 6})).unwrap();
        let export_params = json!({"standard": "x4"});
        let print_params = json!({"printer": "Office", "standard": "x4"});
        // The creation date is in whole seconds: an attempt can straddle a second boundary.
        let mut matched = false;
        for _ in 0..3 {
            let exported = s.execute("file.exportPdf", &export_params).unwrap();
            let mut printed = Vec::new();
            let r = print_with(&mut s, &print_params, |pdf, _| {
                printed = pdf.to_vec();
                Ok("request id is Office-1".into())
            })
            .unwrap();
            assert_eq!(r["warnings"], exported["warnings"]);
            assert!(r["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("soft effects")), "{r}");
            if printed == super::super::file::base64_decode(exported["base64"].as_str().unwrap()) {
                matched = true;
                break;
            }
        }
        assert!(matched, "File › Print spools Export PDF's bytes");
    }
}
