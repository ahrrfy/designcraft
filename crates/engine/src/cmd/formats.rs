//! Honest output formats: a file's extension must describe the bytes we write.
use super::{CommandSpec, always, bad, cmd};
use crate::Result;
use serde_json::{Value, json};

pub(super) fn checked_path(command: &str, path: &str, extension: &str) -> Result<String> {
    if path.is_empty() || path.len() > 32768 || std::path::Path::new(path).file_name().is_none() {
        return Err(bad(command, "output needs a bounded file path"));
    }
    match std::path::Path::new(path).extension().and_then(|s| s.to_str()) {
        None => Ok(format!("{path}.{extension}")),
        Some(ext) if ext.eq_ignore_ascii_case(extension) => Ok(path.to_string()),
        Some(ext) if ext.eq_ignore_ascii_case("indd") || ext.eq_ignore_ascii_case("indt") => Err(bad(
            command,
            "native INDD/INDT export is unavailable in the Rust engine; export IDML for editable interchange, or use an explicitly configured external InDesign bridge",
        )),
        Some(_) => Err(bad(command, format!("output extension must be .{extension}; the exporter will not disguise another format"))),
    }
}

pub(super) fn native_path(path: &str) -> Result<String> {
    if path.is_empty() || path.len() > 32768 || std::path::Path::new(path).file_name().is_none() {
        return Err(bad("file.save", "output needs a bounded file path"));
    }
    if std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|ext| ["indd", "indt", "idml", "pdf"].iter().any(|known| ext.eq_ignore_ascii_case(known)))
    {
        return Err(bad(
            "file.save",
            "native saves require .designcraft; use file.exportIdml or file.exportPdf for interchange. Native INDD/INDT writing is unavailable in the Rust engine",
        ));
    }
    Ok(super::file::with_document_extension(path))
}

pub(super) fn optional_path(command: &str, p: &Value, extension: &str) -> Result<Option<String>> {
    match p.get("path") {
        None => Ok(None),
        Some(Value::String(path)) => checked_path(command, path, extension).map(Some),
        Some(_) => Err(bad(command, "path must be a string when provided")),
    }
}

fn catalog() -> Value {
    json!({"formats":[
        {"id":"native","extension":"designcraft","command":"file.saveAs","available":true,"editable":true,"description":"Native DesignCraft document, preserving engine text, paths and document structure"},
        {"id":"pdf","extension":"pdf","command":"file.exportPdf","available":true,"editable":false,"description":"Final page output; vector/text content where supported, with export warnings for approximated features"},
        {"id":"idml","extension":"idml","command":"file.exportIdml","available":true,"editable":true,"description":"Editable interchange; target-application rendering and unsupported feature parity require verification"},
        {"id":"indd","extension":"indd","available":false,"editable":true,"description":"No native INDD writer in the Rust engine; an external installed InDesign bridge must be explicitly configured and verified separately"}
    ]})
}
pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "file.exportFormats","Export Formats",[],None,"{} → {formats: available native, PDF and IDML outputs; honest INDD availability}",always,|_,_| Ok(catalog())),
    ]
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::Session;

    #[test]
    fn output_extensions_are_honest_and_rejected_outputs_preserve_existing_files() {
        let directory = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let native = directory.path().join("working.designcraft");
        s.execute("file.saveAs", &json!({"path":native})).unwrap();
        let original = std::fs::read(&native).unwrap();
        let document = s.doc().unwrap().doc.clone();
        for (command, extension) in [
            ("file.exportPdf", "idml"),
            ("file.exportPdf", "InDd"),
            ("file.exportIdml", "pdf"),
            ("file.exportIdml", "INDT"),
            ("file.saveAs", "indd"),
            ("file.saveACopy", "pdf"),
            ("file.printBooklet", "indd"),
        ] {
            let path = directory.path().join(format!("must-not-change.{extension}"));
            std::fs::write(&path, b"sentinel").unwrap();
            assert!(s.execute(command, &json!({"path":path})).is_err(), "{command}");
            assert_eq!(std::fs::read(&path).unwrap(), b"sentinel");
            assert!(std::sync::Arc::ptr_eq(&document, &s.doc().unwrap().doc));
        }
        for command in ["file.save", "file.saveAs", "file.exportPdf", "file.exportIdml"] {
            assert!(s.execute(command, &json!({"path":17})).is_err(), "{command}");
            assert!(s.execute(command, &json!({"path":""})).is_err(), "{command}");
        }
        assert_eq!(std::fs::read(&native).unwrap(), original);
        assert_eq!(checked_path("file.exportPdf", "Report.PDF", "pdf").unwrap(), "Report.PDF");
        assert!(native_path("Report.indd").is_err());
        let catalog = s.execute("file.exportFormats", &json!({})).unwrap();
        assert_eq!(catalog["formats"].as_array().unwrap().iter().find(|entry| entry["id"] == "indd").unwrap()["available"], false);
    }

    #[test]
    fn native_and_idml_reopen_editable_text_and_paths_while_pdf_is_actual_a4_output() {
        let directory = tempfile::tempdir().unwrap();
        let mut s = Session::new();
        s.execute("file.new", &json!({"preset":"A4","facingPages":false})).unwrap();
        let text = s.execute("frame.create", &json!({"rect":[54,54,400,120],"content":"text","text":"Editable lesson","caret":false})).unwrap();
        s.execute("object.label", &json!({"ids":[text["id"]],"label":"lesson-text"})).unwrap();
        let path = s.execute("path.create", &json!({"anchors":[{"p":[54,160]},{"p":[100,160]},{"p":[78,195]}],"closed":true})).unwrap();
        s.execute("object.label", &json!({"ids":[path["id"]],"label":"editable-diagram"})).unwrap();
        for (command, stem, extension) in
            [("file.saveAs", "native", "designcraft"), ("file.exportIdml", "interchange", "idml"), ("file.exportPdf", "final", "pdf")]
        {
            let result = s.execute(command, &json!({"path":directory.path().join(stem)})).unwrap();
            assert_eq!(std::path::Path::new(result["path"].as_str().unwrap()), directory.path().join(format!("{stem}.{extension}")));
            if extension == "idml" {
                assert!(
                    result["warnings"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|warning| warning.as_str().is_some_and(|warning| warning.contains("Script labels")))
                );
            }
        }
        let pdf = std::fs::read(directory.path().join("final.pdf")).unwrap();
        assert!(pdf.starts_with(b"%PDF-"));
        assert_eq!(designcraft_render::pdf_page_count(&pdf), Some(1));
        let (w, h) = designcraft_render::pdf_page_size(&pdf, 0).unwrap();
        assert!((w - 210.0 * 72.0 / 25.4).abs() < 0.05 && (h - 297.0 * 72.0 / 25.4).abs() < 0.05);
        for filename in ["native.designcraft", "interchange.idml"] {
            let mut reopened = Session::new();
            reopened.execute("file.open", &json!({"path":directory.path().join(filename)})).unwrap();
            let document = &reopened.doc().unwrap().doc;
            let text = document
                .spreads
                .iter()
                .flat_map(|spread| &spread.items)
                .find(|item| item.text_frame().is_some_and(|frame| document.story(frame.story).is_some_and(|story| story.text == "Editable lesson")))
                .unwrap();
            let story = text.text_frame().unwrap().story;
            assert_eq!(document.story(story).unwrap().text, "Editable lesson");
            let diagram = document
                .spreads
                .iter()
                .flat_map(|spread| &spread.items)
                .find(|item| !item.is_text_frame() && item.path.subpaths.first().is_some_and(|subpath| subpath.anchors.len() >= 3))
                .unwrap();
            let diagram_id = diagram.id;
            let original_path = diagram.path.clone();
            if filename.ends_with("designcraft") {
                assert_eq!(text.label, "lesson-text");
                assert_eq!(diagram.label, "editable-diagram");
            }
            reopened.execute("story.setText", &json!({"story":story.0,"text":"Edited independently"})).unwrap();
            assert_eq!(reopened.doc().unwrap().doc.story(story).unwrap().text, "Edited independently");
            reopened.execute("edit.undo", &json!({})).unwrap();
            assert_eq!(reopened.doc().unwrap().doc.story(story).unwrap().text, "Editable lesson");
            reopened.execute("path.setAnchor", &json!({"id":diagram_id.0,"anchor":0,"p":[60,170]})).unwrap();
            assert_ne!(reopened.doc().unwrap().doc.item(diagram_id).unwrap().path, original_path);
            reopened.execute("edit.undo", &json!({})).unwrap();
            assert_eq!(reopened.doc().unwrap().doc.item(diagram_id).unwrap().path, original_path);
        }
    }
}
