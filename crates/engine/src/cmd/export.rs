//! File › Export PDF (Print).

use designcraft_pdf::{Marks, PdfOptions, Standard};
use serde_json::{Value, json};
use std::{borrow::Cow, sync::Arc};

use super::{CommandSpec, bad, cmd, has_doc, str_param};
use crate::{EngineError, Result, Session};

mod flatten;
mod formats;
mod pdf;
mod registry;

#[cfg(test)]
use flatten::{rasterize_effects, shadow_coverage};
pub(crate) use pdf::pdf_export;
pub(crate) use pdf::{check_pdf_options, pdf_report};
pub(crate) use pdf::{flatten_ppi, options, print_pdf};
pub use registry::specs;

#[cfg(test)]
mod text_tests;

#[cfg(test)]
mod pdf_fixes_tests {
    use designcraft_doc::{ItemId, PageTransition, TransitionKind};
    use serde_json::json;

    use crate::Session;
    use crate::cmd::base64_decode;

    #[test]
    fn object_style_and_based_on_soft_effects_rasterize_in_pdf_export() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        s.execute("style.object.create", &json!({"name": "Shadow Base", "dropShadow": {"on": true, "distance": 6, "size": 8, "opacity": 0.5}}))
            .unwrap();
        s.execute("style.object.create", &json!({"name": "Shadow Child", "basedOn": "Shadow Base", "fill": "[Black]"})).unwrap();
        let id = ItemId(s.execute("frame.create", &json!({"rect": [72, 72, 180, 180]})).unwrap()["id"].as_u64().unwrap());
        s.execute("style.object.apply", &json!({"name": "Shadow Child", "ids": [id.0]})).unwrap();

        let r = s.execute("file.exportPdf", &json!({})).unwrap();
        assert!(
            r["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("1 object(s) with soft effects")),
            "expected inherited soft-effect rasterisation warning, got: {}",
            r["warnings"]
        );

        // Explicitly disabling the inherited drop shadow on the item must be respected in export
        // rather than re-injecting the object style's shadow.
        s.execute("object.dropShadow", &json!({"ids": [id.0], "on": false})).unwrap();
        let r_disabled = s.execute("file.exportPdf", &json!({})).unwrap();
        assert!(
            !r_disabled["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("soft effects")),
            "explicitly disabled shadow must not be rasterised from style, got: {}",
            r_disabled["warnings"]
        );

        // Clearing overrides restores the style's inherited shadow and exports it again.
        s.execute("style.object.clearOverrides", &json!({"ids": [id.0]})).unwrap();
        let r_restored = s.execute("file.exportPdf", &json!({})).unwrap();
        assert!(
            r_restored["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("1 object(s) with soft effects")),
            "expected restored soft-effect rasterisation warning, got: {}",
            r_restored["warnings"]
        );
    }

    #[test]
    fn facing_pages_export_maps_page_transitions_per_exported_page() {
        let mut s = Session::new();
        s.execute("file.new", &json!({"pages": 3, "facingPages": true})).unwrap();
        s.edit(|d, _| {
            let (s1, p1) = d.page_loc(1).unwrap();
            std::sync::Arc::make_mut(&mut d.spreads[s1]).pages[p1].transition =
                Some(PageTransition { kind: TransitionKind::Dissolve, duration: 1.0, horizontal: true });
            let (s2, p2) = d.page_loc(2).unwrap();
            std::sync::Arc::make_mut(&mut d.spreads[s2]).pages[p2].transition =
                Some(PageTransition { kind: TransitionKind::Wipe, duration: 2.0, horizontal: true });
            Ok(())
        })
        .unwrap();

        let all = s.execute("file.exportPdf", &json!({"spreads": false})).unwrap();
        let all_text = String::from_utf8_lossy(&base64_decode(all["base64"].as_str().unwrap())).into_owned();
        assert!(all_text.contains("/Trans<</Type/Trans/S/Dissolve/D 1.00>>"), "{all_text}");
        assert!(all_text.contains("/Trans<</Type/Trans/S/Wipe/Di 0/D 2.00>>"), "{all_text}");

        let page3_only = s.execute("file.exportPdf", &json!({"pages": "3", "spreads": false})).unwrap();
        let page3_text = String::from_utf8_lossy(&base64_decode(page3_only["base64"].as_str().unwrap())).into_owned();
        assert!(page3_text.contains("/Trans<</Type/Trans/S/Wipe/Di 0/D 2.00>>"), "{page3_text}");
        assert!(!page3_text.contains("/S/Dissolve"), "{page3_text}");
    }

    #[test]
    fn pdfx4_and_pdfa2b_skip_disallowed_post_validation_mutations_with_warnings() {
        let mut s = Session::new();
        s.execute("file.new", &json!({})).unwrap();
        let id = s.execute("frame.create", &json!({"rect": [72, 72, 200, 200]})).unwrap()["id"].clone();
        s.execute("object.fill", &json!({"swatch": "[Black]", "ids": [id]})).unwrap();
        s.execute("object.opacity", &json!({"ids": [id], "opacity": 0.5})).unwrap();
        s.edit(|d, _| {
            let (s0, p0) = d.page_loc(0).unwrap();
            std::sync::Arc::make_mut(&mut d.spreads[s0]).pages[p0].transition =
                Some(PageTransition { kind: TransitionKind::Fade, duration: 1.5, horizontal: true });
            Ok(())
        })
        .unwrap();

        s.execute("edit.transparencyBlendSpace", &json!({"space": "rgb"})).unwrap();
        let x4 = s.execute("file.exportPdf", &json!({"standard": "x4", "fullScreen": true, "advanceSeconds": 4})).unwrap();
        let x4_bytes = base64_decode(x4["base64"].as_str().unwrap());
        let x4_errs = designcraft_pdf::check_pdfx4(&x4_bytes);
        assert!(x4_errs.is_empty(), "PDF/X-4 validation must remain intact: {x4_errs:?}");
        let x4_text = String::from_utf8_lossy(&x4_bytes);
        assert!(!x4_text.contains("/FullScreen") && !x4_text.contains("/Dur ") && !x4_text.contains("/Trans<"), "{x4_text}");
        assert!(!designcraft_pdf::has_rgb_groups(&x4_bytes));
        let x4_warn = x4["warnings"].to_string();
        assert!(x4_warn.contains("RGB transparency blend space was left out"), "{x4_warn}");
        assert!(x4_warn.contains("interactive view options were left out"), "{x4_warn}");
        assert!(x4_warn.contains("page advance timings (/Dur) were left out"), "{x4_warn}");
        assert!(x4_warn.contains("page transitions (/Trans) were left out"), "{x4_warn}");

        s.execute("edit.transparencyBlendSpace", &json!({"space": "cmyk"})).unwrap();
        let a2b = s.execute("file.exportPdf", &json!({"standard": "a2b", "fullScreen": true, "advanceSeconds": 4})).unwrap();
        let a2b_bytes = base64_decode(a2b["base64"].as_str().unwrap());
        let a2b_text = String::from_utf8_lossy(&a2b_bytes);
        assert!(!a2b_text.contains("/FullScreen") && !a2b_text.contains("/Dur ") && !a2b_text.contains("/Trans<"), "{a2b_text}");
        assert!(!a2b_text.contains("/CS/DeviceCMYK"), "{a2b_text}");
        let a2b_warn = a2b["warnings"].to_string();
        assert!(a2b_warn.contains("CMYK transparency blend space was left out"), "{a2b_warn}");
        assert!(a2b_warn.contains("interactive view options were left out"), "{a2b_warn}");
        assert!(a2b_warn.contains("page advance timings (/Dur) were left out"), "{a2b_warn}");
        assert!(a2b_warn.contains("page transitions (/Trans) were left out"), "{a2b_warn}");
    }
}
