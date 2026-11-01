use std::sync::Arc;

use serde_json::{Value, json};

use super::apply::bounded;
use super::schema::{check, schemas};
use super::*;
use crate::Session;

#[test]
fn ai24_production_copy_cannot_duplicate_unvalidated_or_grouped_objects() {
    let mut s = crate::Session::new();
    s.execute("file.new", &serde_json::json!({})).unwrap();
    let before = s.doc().unwrap().doc.clone();
    let result = s.execute(
        "production.apply",
        &serde_json::json!({"outputMode":"native_with_images","commands":[
            {"command":"frame.create","params":{"rect":[60,60,120,120]}},
            {"command":"transform.move","params":{"ids":["$0.id"],"dx":100,"dy":0,"copy":true}}
        ]}),
    );
    assert!(result.is_err(), "copy must be limited to a validated raster leaf placed in this private draft");
    assert_eq!(s.doc().unwrap().doc, before);
}

fn session() -> Session {
    let mut s = Session::new();
    s.execute("file.new", &json!({})).unwrap();
    s
}
fn plan(commands: Value) -> Value {
    json!({"outputMode":"native_only","commands":commands})
}
fn sample() -> Value {
    plan(json!([
        {"command":"frame.create","params":{"rect":[36,36,540,200],"content":"text","text":"نص الدرس","caret":false}},
        {"command":"selection.set","params":{"ids":["$0.id"]}},
        {"command":"type.char","params":{"attrs":{"size":16}}},
        {"command":"type.para","params":{"attrs":{"align":"right","direction":"rightToLeft"}}},
        {"command":"story.setDirection","params":{"story":"$0.story","direction":"rightToLeft"}}
    ]))
}

fn grouped_plan() -> Value {
    let chunks: Vec<_> = ["First chunk", "Second chunk"]
        .into_iter()
        .map(|label| {
            let mut commands = vec![
                json!({"command":"frame.create","params":{"rect":[36,36,72,72],"caret":false}}),
                json!({"command":"object.label","params":{"ids":["$0.id"],"label":label}}),
            ];
            commands.extend((2..300).map(|_| json!({"command":"selection.set","params":{"ids":[]}})));
            json!({"commands":commands,"label":label})
        })
        .collect();
    json!({"outputMode":"native_only","chunks":chunks,"label":"Complete review group"})
}

#[test]
fn grouped_production_publishes_over_512_actions_with_independent_references_and_one_undo() {
    let mut s = session();
    s.execute("layout.pages.insert", &json!({"count":3})).unwrap();
    let before = s.doc().unwrap().clone();
    let result = s.execute("production.applyGroup", &grouped_plan()).unwrap();
    assert_eq!((result["groups"].as_u64(), result["steps"].as_u64()), (Some(2), Some(600)));
    let finished = s.doc().unwrap();
    assert_eq!(finished.doc.page_count(), before.doc.page_count(), "requested pages are preserved");
    let labels: Vec<_> = finished.doc.spreads.iter().flat_map(|sp| sp.items.iter()).map(|it| it.label.as_str()).collect();
    assert_eq!(labels, ["First chunk", "Second chunk"], "$0 starts again in each chunk");
    assert_eq!(finished.history.undo.len(), before.history.undo.len() + 1);
    assert_eq!(finished.history.undo.last().unwrap().label, "Complete review group");
    s.execute("edit.undo", &json!({})).unwrap();
    assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    s.execute("edit.redo", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().doc.spreads.iter().map(|sp| sp.items.len()).sum::<usize>(), 2);
}

#[test]
fn grouped_production_later_failure_preserves_live_document_history_selection_and_revision() {
    let mut s = session();
    s.execute("frame.create", &json!({"rect":[0,0,50,50]})).unwrap();
    s.execute("edit.undo", &json!({})).unwrap();
    let before = s.doc().unwrap().clone();
    let journal = s.journal.len();
    let mut p = grouped_plan();
    p["chunks"][1]["commands"][1]["params"]["ids"] = json!(["$0.missing"]);
    let error = s.execute("production.applyGroup", &p).unwrap_err();
    assert!(!error.to_string().contains("unknown command"), "the second chunk must be validated: {error}");
    let after = s.doc().unwrap();
    assert!(Arc::ptr_eq(&before.doc, &after.doc));
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.history.undo.len(), before.history.undo.len());
    assert_eq!(after.history.redo.len(), before.history.redo.len());
    assert_eq!(format!("{:?}", after.selection), format!("{:?}", before.selection));
    assert_eq!(s.journal.len(), journal);
}

fn grouped_facing_setup() -> Value {
    json!({"outputMode":"native_only","documentSetup":{"facingPages":true,"binding":"rightToLeft"},"label":"Facing review group","chunks":[
        {"commands":[
            {"command":"frame.create","params":{"spread":1,"rect":[420,30,520,130]}},
            {"command":"object.label","params":{"ids":["$0.id"],"label":"Absolute page two"}}
        ]},
        {"commands":[
            {"command":"frame.create","params":{"spread":1,"rect":[20,30,120,130]}},
            {"command":"object.label","params":{"ids":["$0.id"],"label":"Absolute page three"}}
        ]}
    ]})
}

#[test]
fn grouped_document_setup_and_mapped_chunks_publish_atomically_with_one_undo() {
    let mut s = session();
    s.execute("file.new", &json!({"width":400,"height":600,"pages":6,"facingPages":false})).unwrap();
    let cover = s.execute("frame.create", &json!({"spread":0,"rect":[20,30,120,130]})).unwrap();
    s.execute("object.label", &json!({"ids":[cover["id"]],"label":"Existing cover"})).unwrap();
    let before = s.doc().unwrap().clone();
    let page_ids: Vec<_> = before.doc.pages().map(|(_, _, page)| page.id).collect();
    let mut parameters = grouped_facing_setup();
    let mut preview = Session::new();
    preview.add_document(before.clone());
    preview.execute("layout.documentSetup", &parameters["documentSetup"]).unwrap();
    let mut preview_parameters = parameters.clone();
    preview_parameters.as_object_mut().unwrap().remove("documentSetup");
    preview.execute("production.applyGroup", &preview_parameters).unwrap();
    assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    parameters["expectedDocumentUid"] = json!(before.uid);
    parameters["expectedRevision"] = json!(before.revision);
    let result = s.execute("production.applyGroup", &parameters).unwrap();
    assert_eq!((result["groups"].as_u64(), result["steps"].as_u64()), (Some(2), Some(4)));
    let after = s.doc().unwrap();
    assert!(after.doc.settings.facing_pages && after.doc.settings.right_to_left_binding);
    assert_eq!((after.doc.page_count(), after.doc.spreads.len()), (6, 4));
    assert_eq!(after.doc.pages().map(|(_, _, page)| page.id).collect::<Vec<_>>(), page_ids);
    assert_eq!(after.doc.item(crate::doc::ItemId(cover["id"].as_u64().unwrap())), before.doc.item(crate::doc::ItemId(cover["id"].as_u64().unwrap())));
    assert_eq!(after.doc.spreads, preview.doc().unwrap().doc.spreads);
    assert_eq!(after.history.undo.len(), before.history.undo.len() + 1);
    assert_eq!(after.history.undo.last().unwrap().label, "Facing review group");
    s.execute("edit.undo", &json!({})).unwrap();
    assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    assert!(!s.doc().unwrap().doc.settings.facing_pages);
    assert_eq!(s.doc().unwrap().doc.spreads.len(), 6);
    assert_eq!(s.doc().unwrap().doc.spreads.iter().map(|spread| spread.items.len()).sum::<usize>(), 1);
}

#[test]
fn grouped_document_setup_rejects_other_fields_or_types_without_live_changes() {
    let mut s = session();
    let before = s.doc().unwrap().clone();
    let journal = s.journal.len();
    for setup in [
        json!(null),
        json!([]),
        json!(true),
        json!({"pages":2}),
        json!({"width":400}),
        json!({"facingPages":"true"}),
        json!({"facingPages":null}),
        json!({"binding":"vertical"}),
        json!({"binding":true}),
        json!({"binding":f64::NAN}),
    ] {
        let mut parameters = grouped_facing_setup();
        parameters["documentSetup"] = setup;
        let error = s.execute("production.applyGroup", &parameters).unwrap_err();
        assert!(error.to_string().contains("documentSetup"), "{error}");
        let after = s.doc().unwrap();
        assert!(Arc::ptr_eq(&before.doc, &after.doc));
        assert_eq!(before.revision, after.revision);
        assert_eq!(before.history.undo.len(), after.history.undo.len());
        assert_eq!(s.journal.len(), journal);
    }
}

#[test]
fn grouped_document_setup_later_failure_and_stale_pins_leave_original_state_untouched() {
    let mut s = session();
    s.execute("file.new", &json!({"width":400,"height":600,"pages":6,"facingPages":false})).unwrap();
    let before = s.doc().unwrap().clone();
    let journal = s.journal.len();
    let mut parameters = grouped_facing_setup();
    parameters["chunks"][1]["commands"][1]["params"]["ids"] = json!(["$0.missing"]);
    let error = s.execute("production.applyGroup", &parameters).unwrap_err();
    assert!(error.to_string().contains("missing"), "{error}");
    assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    for key in ["expectedDocumentUid", "expectedRevision"] {
        let mut parameters = grouped_facing_setup();
        parameters[key] = json!(if key == "expectedDocumentUid" { before.uid + 1 } else { before.revision + 1 });
        assert!(s.execute("production.applyGroup", &parameters).unwrap_err().to_string().contains(key));
        let after = s.doc().unwrap();
        assert!(Arc::ptr_eq(&before.doc, &after.doc));
        assert_eq!(before.revision, after.revision);
        assert_eq!(before.history.undo.len(), after.history.undo.len());
        assert_eq!(before.history.redo.len(), after.history.redo.len());
        assert_eq!(before.selection, after.selection);
    }
    assert_eq!(s.journal.len(), journal);
}

#[test]
fn grouped_production_rejects_stale_identity_and_preserves_single_plan_limits_and_privileges() {
    let mut s = session();
    let before = s.doc().unwrap().clone();
    for key in ["expectedDocumentUid", "expectedRevision"] {
        let mut p = grouped_plan();
        p[key] = json!(if key == "expectedDocumentUid" { before.uid + 1 } else { before.revision + 1 });
        let error = s.execute("production.applyGroup", &p).unwrap_err();
        assert!(error.to_string().contains(key), "{error}");
        assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    }
    let mut p = grouped_plan();
    p["chunks"][0]["commands"] = Value::Array(vec![json!({"command":"selection.set","params":{"ids":[]}}); MAX_ACTIONS + 1]);
    assert!(s.execute("production.applyGroup", &p).unwrap_err().to_string().contains("512"));
    for command in ["file.open", "production.apply", "production.applyGroup", "script.run"] {
        p["chunks"][0]["commands"] = json!([{"command":command,"params":{}}]);
        assert!(s.execute("production.applyGroup", &p).unwrap_err().to_string().contains("not permitted"));
        assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    }
    let catalog = s.execute("production.catalog", &json!({})).unwrap();
    assert!(catalog["commands"].as_array().unwrap().iter().all(|c| c["command"] != "production.applyGroup"));
}

#[test]
fn grouped_production_bounds_group_count_and_rejects_unrecognized_envelopes() {
    let mut s = session();
    let before = s.doc().unwrap().doc.clone();
    let chunk = json!({"commands":[{"command":"selection.set","params":{"ids":[]}}]});
    for chunks in [json!([]), json!(null), Value::Array(vec![chunk.clone(); 1001])] {
        let error = s.execute("production.applyGroup", &json!({"chunks":chunks,"outputMode":"native_only"})).unwrap_err();
        assert!(error.to_string().contains("chunks"), "{error}");
    }
    for p in [
        json!({"chunks":[chunk],"outputMode":"native_only","allowedCommands":["file.open"]}),
        json!({"chunks":[{"commands":[],"outputMode":"native_with_images"}],"outputMode":"native_only"}),
    ] {
        let error = s.execute("production.applyGroup", &p).unwrap_err();
        assert!(error.to_string().contains("unknown"), "{error}");
        assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
    }
}

#[test]
fn grouped_production_preflights_later_chunks_before_publication() {
    let mut s = session();
    let before = s.doc().unwrap().clone();
    let p = json!({"outputMode":"native_only","requirePreflight":true,"minPpi":150,"chunks":[
        {"commands":[{"command":"frame.create","params":{"rect":[72,72,100,100]}}]},
        {"commands":[{"command":"frame.create","params":{"rect":[136,136,140,140],"content":"text","text":"overset text ".repeat(100)}}]}
    ]});
    let error = s.execute("production.applyGroup", &p).unwrap_err();
    assert!(error.to_string().contains("overset"), "{error}");
    assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    assert_eq!(s.doc().unwrap().revision, before.revision);
    assert_eq!(s.doc().unwrap().history.undo.len(), before.history.undo.len());
}

#[test]
fn grouped_production_cancellation_after_an_entire_chunk_keeps_live_state_pristine() {
    let mut s = session();
    s.execute("frame.create", &json!({"rect":[0,0,50,50]})).unwrap();
    s.execute("edit.undo", &json!({})).unwrap();
    let before = s.doc().unwrap().clone();
    let journal = s.journal.len();
    let mut checks = 0usize;
    let error = apply_action_groups(&mut s, &grouped_plan(), || {
        checks += 1;
        checks >= 350
    })
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    assert!(checks > 300, "the first chunk was staged before cancellation");
    let after = s.doc().unwrap();
    assert!(Arc::ptr_eq(&before.doc, &after.doc));
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.history.undo.len(), before.history.undo.len());
    assert_eq!(after.history.redo.len(), before.history.redo.len());
    assert_eq!(format!("{:?}", after.selection), format!("{:?}", before.selection));
    assert_eq!(s.journal.len(), journal);
}

#[test]
fn native_production_has_one_labeled_undo_and_validates_without_edits() {
    let mut s = session();
    let before = s.doc().unwrap().doc.clone();
    let journal = s.journal.len();
    assert_eq!(s.execute("production.validate", &sample()).unwrap()["valid"], true);
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
    assert_eq!(s.journal.len(), journal);
    let mut p = sample();
    p["label"] = json!("School lesson");
    let r = s.execute("production.apply", &p).unwrap();
    assert_eq!(r["steps"], 5);
    assert_eq!(s.doc().unwrap().history.undo.len(), 1);
    assert_eq!(s.doc().unwrap().history.undo[0].label, "School lesson");
    assert_eq!(s.doc().unwrap().doc.spreads[0].items.len(), 1);
    s.execute("edit.undo", &json!({})).unwrap();
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
    s.execute("edit.redo", &json!({})).unwrap();
    assert_eq!(s.doc().unwrap().doc.spreads[0].items.len(), 1);
}

#[test]
fn capabilities_cannot_be_broadened_or_nested() {
    let mut s = session();
    let before = s.doc().unwrap().doc.clone();
    for command in [
        "file.open",
        "file.new",
        "file.print",
        "document.exportPdf",
        "library.open",
        "script.run",
        "production.apply",
        "edit.undo",
        "data.preview",
        "object.pasteSvg",
        "prefs.set",
    ] {
        let mut p = plan(json!([{"command":command,"params":{}}]));
        p["allowedCommands"] = json!([command]);
        assert!(s.execute("production.apply", &p).is_err(), "{command}");
        assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
    }
    let mut p = sample();
    p["allowedCommands"] = json!(["frame.create"]);
    assert!(s.execute("production.apply", &p).is_err());
    assert_eq!(s.doc().unwrap().history.undo.len(), 0);
}

#[test]
fn ordinary_errors_resolved_types_and_cancellation_leave_every_live_state_pristine() {
    let mut s = session();
    s.execute("frame.create", &json!({"rect":[0,0,50,50]})).unwrap();
    s.execute("edit.undo", &json!({})).unwrap();
    let before = s.doc().unwrap().clone();
    let journal = s.journal.len();
    for second in [
        json!({"command":"object.fill","params":{"ids":["$0.id"],"swatch":"missing swatch"}}),
        json!({"command":"object.fill","params":{"ids":["$0"],"swatch":"[Black]"}}),
        json!({"command":"transform.move","params":{"ids":["$0.id"],"dx":"8","dy":0}}),
        json!({"command":"type.char","params":{"attrs":{"size":12,"unsupported":true}}}),
    ] {
        let p = plan(json!([{"command":"frame.create","params":{"rect":[0,0,50,50]}},second]));
        assert!(s.execute("production.apply", &p).is_err());
        assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
        assert_eq!(format!("{:?}", s.doc().unwrap().selection), format!("{:?}", before.selection));
        assert_eq!(s.doc().unwrap().history.redo.len(), before.history.redo.len());
        assert_eq!(s.doc().unwrap().revision, before.revision);
        assert_eq!(s.journal.len(), journal);
    }
    let mut checks = 0;
    assert!(
        apply_actions(&mut s, &sample(), || {
            checks += 1;
            checks >= 2
        })
        .is_err()
    );
    assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    assert_eq!(s.doc().unwrap().history.redo.len(), 1);
    assert!(
        apply_actions(&mut s, &plan(json!([{"command":"frame.create","params":{"rect":[0,0,50,50]}}])), || {
            checks += 1;
            checks >= 4
        })
        .is_err()
    );
    assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
}

#[test]
fn native_only_rejects_pixels_and_image_mode_never_accepts_svg_or_paths() {
    let mut s = session();
    for params in [
        json!({"name":"image.png","base64":""}),
        json!({"name":"image.svg","base64":super::super::base64_encode(b"<svg><image href='x'/></svg>")}),
        json!({"path":"private.png","name":"image.png","base64":""}),
    ] {
        let mut p = plan(json!([{"command":"file.place","params":params}]));
        assert!(s.execute("production.apply", &p).is_err());
        p["outputMode"] = json!("native_with_images");
        assert!(s.execute("production.apply", &p).is_err());
    }
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let p = json!({"outputMode":"native_with_images","commands":[{"command":"file.place","params":{"name":"illustration.png","base64":png,"x":36,"y":36,"width":100}}]});
    assert!(s.execute("production.apply", &p).is_ok());
    assert_eq!(s.doc().unwrap().history.undo.len(), 1);
}

fn inline_raster_png(width: u32, height: u32, padding: usize, marker: u8) -> Vec<u8> {
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], bytes: &[u8]) {
        let mut table = [0u32; 256];
        for (index, entry) in table.iter_mut().enumerate() {
            let mut crc = index as u32;
            for _ in 0..8 {
                crc = if crc & 1 == 0 { crc >> 1 } else { (crc >> 1) ^ 0xedb8_8320 };
            }
            *entry = crc;
        }
        let mut crc = u32::MAX;
        for &byte in kind.iter().chain(bytes) {
            crc = (crc >> 8) ^ table[((crc ^ u32::from(byte)) & 255) as usize];
        }
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(bytes);
        out.extend_from_slice(&(!crc).to_be_bytes());
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[1, 0, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &header);
    let raw_bytes = (width as usize).div_ceil(8).checked_add(1).unwrap().checked_mul(height as usize).unwrap();
    let mut remaining = raw_bytes;
    let mut compressed = vec![0x78, 0x01];
    while remaining > 0 {
        let size = remaining.min(u16::MAX as usize) as u16;
        remaining -= usize::from(size);
        compressed.push(u8::from(remaining == 0));
        compressed.extend_from_slice(&size.to_le_bytes());
        compressed.extend_from_slice(&(!size).to_le_bytes());
        compressed.resize(compressed.len() + usize::from(size), 0);
    }
    compressed.extend_from_slice(&((((raw_bytes % 65_521) as u32) << 16) | 1).to_be_bytes());
    chunk(&mut png, b"IDAT", &compressed);
    chunk(&mut png, b"raSt", &vec![marker; padding]);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn inline_raster_action(bytes: &[u8], x: u32) -> Value {
    json!({"command":"file.place","params":{"name":"generated.png","base64":super::super::base64_encode(bytes),"x":x,"y":36,"width":100}})
}

#[test]
fn inline_raster_large_4k_shared_views_embed_exact_bytes_and_roundtrip_with_one_undo() {
    let bytes = inline_raster_png(4096, 4096, 5 * 1024 * 1024, 7);
    assert!(bytes.len() > 6 * 1024 * 1024 && bytes.len() < 16 * 1024 * 1024);
    assert_eq!(designcraft_images::pixel_size(&bytes), Some((4096, 4096)));
    let mut s = session();
    let before = s.doc().unwrap().clone();
    let commands: Vec<_> = [36, 150, 264].into_iter().map(|x| inline_raster_action(&bytes, x)).collect();
    s.execute("production.apply", &json!({"outputMode":"native_with_images","commands":commands})).unwrap();
    let state = s.doc().unwrap();
    assert_eq!(state.history.undo.len(), before.history.undo.len() + 1);
    assert_eq!(state.doc.assets.len(), 1, "Shared views must retain one source asset");
    let asset = state.doc.assets.values().next().unwrap();
    assert_eq!(asset.data.as_slice(), bytes);
    assert_eq!(asset.pixels, Some((4096, 4096)));
    assert!(asset.link.is_none(), "The generated image must remain embedded");
    let saved = designcraft_format::save(&state.doc).unwrap();
    let reopened = designcraft_format::load(&saved).unwrap();
    assert_eq!(reopened.assets.len(), 1);
    let asset = reopened.assets.values().next().unwrap();
    assert_eq!(asset.data.as_slice(), bytes);
    assert!(asset.link.is_none());
    s.execute("edit.undo", &json!({})).unwrap();
    assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
}

#[test]
fn inline_raster_limits_and_unknown_paths_reject_atomically() {
    let mut s = session();
    let before = s.doc().unwrap().clone();
    let journal = s.journal.len();
    let tiny = inline_raster_png(1, 1, 0, 0);
    let mut path = inline_raster_action(&tiny, 36);
    path["params"]["path"] = json!("private.png");
    let oversized = inline_raster_png(1, 1, 16 * 1024 * 1024, 0);
    let too_many_pixels = inline_raster_png(5000, 4001, 0, 0);
    for rejected in [path, inline_raster_action(&oversized, 36), inline_raster_action(&too_many_pixels, 36)] {
        let p = json!({"outputMode":"native_with_images","commands":[
            {"command":"frame.create","params":{"rect":[36,36,72,72]}},rejected
        ]});
        assert!(s.execute("production.apply", &p).is_err());
        let after = s.doc().unwrap();
        assert!(Arc::ptr_eq(&before.doc, &after.doc));
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.history.undo.len(), before.history.undo.len());
        assert_eq!(s.journal.len(), journal);
    }
}

#[test]
fn inline_raster_unique_source_aggregate_limits_reject_atomically() {
    let mut s = session();
    let before = s.doc().unwrap().clone();
    for (width, height, padding, expected) in [(4096, 4096, 1, "40 million"), (1, 1, 11 * 1024 * 1024, "32 MiB")] {
        let commands: Vec<_> = (0..3).map(|marker| inline_raster_action(&inline_raster_png(width, height, padding, marker), 36)).collect();
        let error = s.execute("production.apply", &json!({"outputMode":"native_with_images","commands":commands})).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
        assert_eq!(s.doc().unwrap().revision, before.revision);
        assert_eq!(s.doc().unwrap().history.undo.len(), before.history.undo.len());
    }
}

#[test]
fn restricted_graphic_fitting_preserves_frame_and_rejects_hostile_parameters() {
    let mut s = session();
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let mut p = json!({"outputMode":"native_with_images","commands":[
        {"command":"frame.create","params":{"rect":[54,54,154,154],"content":"graphic"}},
        {"command":"file.place","params":{"frame":"$0.id","name":"illustration.png","base64":png}},
        {"command":"object.fittingOptions","params":{"ids":["$0.id"],"fitting":"fillProportionally","align":4,"crop":[0,25,0,25]}}
    ]});
    s.execute("production.apply", &p).unwrap();
    let item = &s.doc().unwrap().doc.spreads[0].items[0];
    let designcraft_doc::Content::Graphic(g) = &item.content else { panic!("Expected graphic") };
    assert_eq!(g.crop, [0.0, 25.0, 0.0, 25.0]);
    assert_eq!(item.inner_bounds(), designcraft_geom::Rect::new(54.0, 54.0, 154.0, 154.0));
    for params in [
        json!({"ids":["$0.id"],"fitting":"fitFrameToContent"}),
        json!({"ids":["$0.id"],"fitting":"fillProportionally","crop":[0,null,0,0]}),
        json!({"ids":["$0.id"],"fitting":"fillProportionally","crop":[0,1e300,0,0]}),
        json!({"ids":["$0.id"],"fitting":"fillProportionally","align":9}),
        json!({"ids":["$0.id"],"fitting":"fillProportionally","path":"private.jpg"}),
    ] {
        p["commands"][2]["params"] = params;
        let before = s.doc().unwrap().doc.clone();
        assert!(s.execute("production.apply", &p).is_err());
        assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
    }
}

#[test]
fn stale_documents_and_hostile_plan_bounds_are_rejected() {
    let mut s = session();
    let mut p = sample();
    p["expectedDocumentUid"] = json!(s.doc().unwrap().uid);
    p["expectedRevision"] = json!(s.doc().unwrap().revision + 1);
    assert!(s.execute("production.apply", &p).is_err());
    p["expectedRevision"] = json!(s.doc().unwrap().revision);
    assert!(s.execute("production.validate", &p).is_ok());
    let too_many = plan(Value::Array(vec![json!({"command":"selection.set","params":{"ids":[]}}); MAX_ACTIONS + 1]));
    assert!(s.execute("production.validate", &too_many).is_err());
    let negative = plan(json!([{"command":"frame.create","params":{"rect":[0,0,50,50],"spread":-1}}]));
    assert!(s.execute("production.apply", &negative).is_err());
    let mut nested = json!(0);
    for _ in 0..30 {
        nested = json!([nested]);
    }
    assert!(bounded(&nested).is_err());
    let c = s.execute("production.catalog", &json!({})).unwrap();
    assert!(c["commands"].as_array().unwrap().iter().all(|v| v["paramsSchema"]["additionalProperties"] == false));
}

#[test]
fn preflight_rejects_overset_on_the_draft_preserving_live_document_and_history() {
    let mut s = session();
    s.execute("frame.create", &json!({"rect":[0,0,50,50]})).unwrap();
    s.execute("edit.undo", &json!({})).unwrap();
    let before = s.doc().unwrap().clone();
    let journal = s.journal.len();
    let p = json!({"outputMode":"native_only","requirePreflight":true,"minPpi":150,
        "commands":[{"command":"frame.create","params":{"rect":[36,36,40,40],"content":"text","text":"overset text ".repeat(100)}}]});
    let err = s.execute("production.apply", &p).unwrap_err();
    assert!(err.to_string().contains("overset"), "{err}");
    assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
    assert_eq!(s.doc().unwrap().history.undo.len(), before.history.undo.len());
    assert_eq!(s.doc().unwrap().history.redo.len(), before.history.redo.len());
    assert_eq!(s.doc().unwrap().revision, before.revision);
    assert_eq!(format!("{:?}", s.doc().unwrap().selection), format!("{:?}", before.selection));
    assert_eq!(s.journal.len(), journal);
    assert!(s.execute("production.validate", &p).is_err());
    let success = json!({"outputMode":"native_only","requirePreflight":true,"minPpi":300,
        "commands":[{"command":"frame.create","params":{"rect":[36,36,200,200],"content":"unassigned"}},
            {"command":"object.fill","params":{"ids":["$0.id"],"swatch":"[Paper]"}}]});
    s.execute("production.apply", &success).unwrap();
    assert_eq!(s.doc().unwrap().history.undo.len(), before.history.undo.len() + 1);
    assert!(s.doc().unwrap().history.redo.is_empty());
    s.execute("edit.undo", &json!({})).unwrap();
    assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
}

#[test]
fn preflight_options_are_strictly_typed_and_bounded() {
    let mut s = session();
    let before = s.doc().unwrap().doc.clone();
    for (key, value) in [
        ("requirePreflight", json!("true")),
        ("requirePreflight", Value::Null),
        ("minPpi", json!(0)),
        ("minPpi", json!(2401)),
        ("minPpi", json!("150")),
    ] {
        let mut p = sample();
        p[key] = value;
        assert!(s.execute("production.apply", &p).is_err());
        assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
    }
}

fn layout_font() -> &'static str {
    static LOADED: std::sync::Once = std::sync::Once::new();
    LOADED.call_once(|| {
        designcraft_fonts::FontDb::global().add_font(designcraft_fonts::testing::font_with("DC Production Layout Guard", &['A', 'B']).unwrap());
    });
    "DC Production Layout Guard"
}

fn layout_text(s: &mut Session, rect: [f64; 4]) -> Value {
    let item = s.execute("frame.create", &json!({"rect":rect,"content":"text","text":"A","caret":false})).unwrap();
    s.execute("type.char", &json!({"attrs":{"fontFamily":layout_font(),"size":12}})).unwrap();
    item
}

fn layout_plan(rectangles: &[[f64; 4]]) -> Value {
    let mut commands = Vec::new();
    for rect in rectangles {
        commands.push(json!({"command":"frame.create","params":{"rect":rect,"content":"text","text":"A","caret":false}}));
        commands.push(json!({"command":"type.char","params":{"attrs":{"fontFamily":layout_font(),"size":12}}}));
    }
    json!({"outputMode":"native_only","requirePreflight":true,"commands":commands})
}

#[test]
fn gated_text_overlap_and_unsafe_margins_leave_no_live_changes() {
    let mut s = session();
    let before = s.doc().unwrap().clone();
    for (rects, kind) in
        [(vec![[72.0, 72.0, 250.0, 150.0], [180.0, 100.0, 300.0, 180.0]], "textOverlap"), (vec![[20.0, 72.0, 150.0, 120.0]], "textOutsideMargins")]
    {
        let err = s.execute("production.apply", &layout_plan(&rects)).unwrap_err();
        assert!(err.to_string().contains(kind), "{err}");
        assert!(err.to_string().contains("\"item\"") && err.to_string().contains("\"page\":1"), "{err}");
        assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
        assert_eq!(s.doc().unwrap().history.undo.len(), before.history.undo.len());
        assert_eq!(s.doc().unwrap().revision, before.revision);
    }
    s.execute("production.apply", &layout_plan(&[[72.0, 72.0, 250.0, 120.0], [72.0, 150.0, 250.0, 198.0]])).unwrap();
    assert_eq!(s.doc().unwrap().history.undo.len(), 1);
    s.execute("edit.undo", &json!({})).unwrap();
    assert!(Arc::ptr_eq(&before.doc, &s.doc().unwrap().doc));
}

#[test]
fn unchanged_old_overlap_allows_unrelated_work_but_story_changes_are_checked() {
    let mut s = session();
    let first = layout_text(&mut s, [72.0, 72.0, 250.0, 150.0]);
    layout_text(&mut s, [180.0, 100.0, 300.0, 180.0]);
    let unrelated = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
        {"command":"line.create","params":{"a":[72,300],"b":[400,300]}}]});
    s.execute("production.apply", &unrelated).unwrap();
    let before = s.doc().unwrap().doc.clone();
    let edits = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
        {"command":"story.setText","params":{"story":first["story"],"text":"B"}}]});
    let err = s.execute("production.apply", &edits).unwrap_err();
    assert!(err.to_string().contains("textOverlap"), "{err}");
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
}

#[test]
fn group_parent_transforms_are_applied_to_text_layout_bounds() {
    let mut s = session();
    let a = layout_text(&mut s, [72.0, 72.0, 200.0, 140.0]);
    let shape = s.execute("frame.create", &json!({"rect":[72,72,80,80]})).unwrap();
    let group = s.execute("object.group", &json!({"ids":[a["id"],shape["id"]]})).unwrap();
    layout_text(&mut s, [300.0, 72.0, 450.0, 140.0]);
    let before = s.doc().unwrap().doc.clone();
    let move_group = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
        {"command":"transform.move","params":{"ids":[group["id"]],"dx":230,"dy":0}}]});
    let err = s.execute("production.apply", &move_group).unwrap_err();
    assert!(err.to_string().contains("textOverlap"), "{err}");
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
}

#[test]
fn vetted_existing_document_editing_commands_apply_and_reject_hostile_params() {
    let mut s = session();
    let card_a = s.execute("frame.create", &json!({"rect":[72,72,172,132],"content":"unassigned"})).unwrap()["id"].as_u64().unwrap();
    let card_b = s.execute("frame.create", &json!({"rect":[72,160,172,220],"content":"unassigned"})).unwrap()["id"].as_u64().unwrap();
    let card_c = s.execute("frame.create", &json!({"rect":[72,260,172,320],"content":"unassigned"})).unwrap()["id"].as_u64().unwrap();
    let obsolete = s.execute("frame.create", &json!({"rect":[300,72,400,132],"content":"unassigned"})).unwrap()["id"].as_u64().unwrap();
    let text_frame = s.execute("frame.create", &json!({"rect":[200,72,500,150],"content":"text","text":"Old Title Here","caret":false})).unwrap();
    let table_frame = s.execute("frame.create", &json!({"rect":[200,180,500,320],"content":"text","text":"","caret":true})).unwrap();
    let table_story = table_frame["story"].as_u64().unwrap();
    let table = s.execute("table.insert", &json!({"rows":2,"cols":2})).unwrap()["table"].as_u64().unwrap();
    s.execute("selection.set", &json!({"ids":[]})).unwrap();
    assert!(s.doc().unwrap().selection.text.is_none());
    let group = s.execute("object.group", &json!({"ids":[card_a,card_b]})).unwrap()["id"].as_u64().unwrap();

    let edit_plan = json!({"outputMode":"native_only","commands":[
        {"command":"object.ungroup","params":{"ids":[group]}},
        {"command":"edit.clear","params":{"ids":[obsolete]}},
        {"command":"object.cornerOptions","params":{"ids":[card_a],"shape":"rounded","size":10}},
        {"command":"object.opacity","params":{"ids":[card_a],"opacity":0.85,"blend":"multiply"}},
        {"command":"object.dropShadow","params":{"ids":[card_a],"on":true,"distance":4,"angle":135,"opacity":0.3,"size":8,"color":"[Black]"}},
        {"command":"object.textWrap","params":{"ids":[card_a],"mode":"boundingBox","offset":6}},
        {"command":"object.align","params":{"ids":[card_a,card_b,card_c],"edge":"left","to":"margins"}},
        {"command":"object.distribute","params":{"ids":[card_a,card_b,card_c],"axis":"vertical","by":"spacing","spacing":16}},
        {"command":"find.change","params":{"find":"Old Title","change":"Updated Title","scope":"document"}},
        {"command":"table.setCell","params":{"story":table_story,"table":table,"row":1,"col":1,"text":"Edited Cell","fill":"[Paper]"}}
    ]});
    s.execute("production.apply", &edit_plan).unwrap();

    let doc = &s.doc().unwrap().doc;
    assert!(doc.item(designcraft_doc::ItemId(obsolete)).is_none(), "edit.clear should delete obsolete item");
    assert!(doc.item(designcraft_doc::ItemId(group)).is_none(), "object.ungroup should dissolve group");
    let a = doc.item(designcraft_doc::ItemId(card_a)).unwrap();
    assert!((a.opacity - 0.85).abs() < 1e-4);
    assert!(a.effects.drop_shadow.on);
    assert_eq!(doc.story(designcraft_doc::StoryId(text_frame["story"].as_u64().unwrap())).unwrap().text, "Updated Title Here");
    let tbl = doc.story(designcraft_doc::StoryId(table_story)).unwrap().tables.get(&table).unwrap();
    assert_eq!(tbl.cell(1, 1).unwrap().text.text, "Edited Cell");
    assert_eq!(tbl.cell(1, 1).unwrap().fill, "[Paper]");

    let before = s.doc().unwrap().doc.clone();
    let bad_find = json!({"outputMode":"native_only","commands":[
        {"command":"find.change","params":{"find":"","change":"X"}}
    ]});
    assert!(s.execute("production.apply", &bad_find).is_err());
    let bad_shadow = json!({"outputMode":"native_only","commands":[
        {"command":"object.dropShadow","params":{"ids":[card_a],"on":true,"color":"MissingSwatch"}}
    ]});
    assert!(s.execute("production.apply", &bad_shadow).is_err());
    let bad_cell_fill = json!({"outputMode":"native_only","commands":[
        {"command":"table.setCell","params":{"story":table_story,"table":table,"row":0,"col":0,"fill":"MissingSwatch"}}
    ]});
    assert!(s.execute("production.apply", &bad_cell_fill).is_err());
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
}

#[test]
fn vetted_full_design_tools_styles_vectors_tables_lists_threading_and_qr_apply_and_guard_bounds() {
    for (id, schema, example) in schemas() {
        assert!(super::super::find_command(id).is_some(), "vetted command `{id}` must exist in engine registry");
        if !example.to_string().contains('$') {
            check(&schema, &example, id).unwrap_or_else(|e| panic!("example for `{id}` failed schema check: {e}"));
        }
    }

    let mut s = session();
    let mut cmds = vec![
        json!({"command":"swatch.create","params":{"name":"Brand Navy","color":[30,58,138]}}),
        json!({"command":"swatch.create","params":{"name":"Brand Gold","color":"#D97706"}}),
        json!({"command":"list.define","params":{"name":"Steps","continueAcrossStories":true}}),
        json!({"command":"style.paragraph.create","params":{"name":"Arabic Body","para":{"align":"right","direction":"rightToLeft","kashidas":true,"listType":"numbers","listName":"Steps","spaceAfter":6},"chars":{"size":13}}}),
        json!({"command":"style.paragraph.edit","params":{"name":"Arabic Body","para":{"spaceBefore":2}}}),
        json!({"command":"style.character.create","params":{"name":"Gold Highlight","chars":{"fill":"Brand Gold","underline":true}}}),
        json!({"command":"style.character.edit","params":{"name":"Gold Highlight","chars":{"tracking":20}}}),
        json!({"command":"style.object.create","params":{"name":"Brand Card","fill":"Brand Navy","dropShadow":{"on":true,"distance":4,"angle":135,"opacity":0.25,"size":6,"color":"[Black]"}}}),
        json!({"command":"frame.create","params":{"rect":[54,54,260,140],"content":"text","text":"الفقرة الأولى\nالفقرة الثانية","caret":false}}),
        json!({"command":"style.paragraph.apply","params":{"name":"Arabic Body","clearOverrides":true}}),
        json!({"command":"style.character.apply","params":{"name":"Gold Highlight"}}),
        json!({"command":"object.fit","params":{"ids":["$8.id"],"mode":"fitFrameToContent","axis":"height"}}),
        json!({"command":"frame.create","params":{"rect":[300,54,500,140],"content":"text","text":"","caret":false}}),
        json!({"command":"text.thread","params":{"from":"$8.id","to":"$12.id"}}),
        json!({"command":"text.unthread","params":{"frame":"$8.id","side":"after"}}),
        json!({"command":"frame.create","params":{"rect":[54,180,154,260],"content":"unassigned"}}),
        json!({"command":"style.object.apply","params":{"name":"Brand Card","ids":["$15.id"]}}),
        json!({"command":"object.gradient","params":{"ids":["$15.id"],"kind":"linear","angle":45,"stops":[{"pos":0.0,"color":"#1E3A8A"},{"pos":1.0,"color":"#D97706"}]}}),
        json!({"command":"object.feather","params":{"ids":["$15.id"],"width":4}}),
        json!({"command":"object.gradientFeather","params":{"ids":["$15.id"],"on":true,"angle":90,"start":10.0,"end":90.0}}),
        json!({"command":"object.effects","params":{"ids":["$15.id"],"opacity":0.9,"blend":"normal","effects":{"outerGlow":{"on":true,"color":"Brand Gold","opacity":0.3,"size":8},"bevel":{"on":true,"size":4,"depth":50,"highlight":"[Paper]","shadow":"[Black]"},"directionalFeather":{"on":true,"widths":[2,2,2,2]}}}}),
        json!({"command":"transform.rotate","params":{"ids":["$15.id"],"angle":5,"ref":4}}),
        json!({"command":"transform.scale","params":{"ids":["$15.id"],"sx":1.05,"sy":1.05}}),
        json!({"command":"transform.flip","params":{"ids":["$15.id"],"axis":"horizontal"}}),
        json!({"command":"edit.stepAndRepeat","params":{"ids":["$15.id"],"count":2,"dx":120,"dy":0}}),
    ];
    cmds.extend([
        json!({"command":"frame.create","params":{"rect":[54,300,134,380],"shape":"ellipse","content":"unassigned"}}),
        json!({"command":"frame.create","params":{"rect":[94,300,174,380],"shape":"ellipse","content":"unassigned"}}),
        json!({"command":"object.pathfinder","params":{"ids":["$25.id","$26.id"],"op":"add"}}),
        json!({"command":"frame.create","params":{"rect":[220,300,280,360],"content":"unassigned"}}),
        json!({"command":"frame.create","params":{"rect":[240,320,260,340],"content":"unassigned"}}),
        json!({"command":"object.makeCompoundPath","params":{"ids":["$28.id","$29.id"]}}),
        json!({"command":"object.releaseCompoundPath","params":{"ids":["$30.id"]}}),
        json!({"command":"object.qrCode","params":{"type":"url","content":"https://designcraft.example","rect":[440,300,520,380],"color":"Brand Navy"}}),
        json!({"command":"frame.create","params":{"rect":[54,420,540,700],"content":"text","text":"","caret":true}}),
        json!({"command":"table.insert","params":{"rows":3,"cols":3,"headerRows":1,"width":480}}),
        json!({"command":"style.table.create","params":{"name":"Report Grid","border":{"weight":1,"color":"Brand Navy"},"altRows":{"first":1,"firstColor":"[Paper]","firstTint":1.0,"next":1,"nextColor":"Brand Gold","nextTint":0.15}}}),
        json!({"command":"style.table.apply","params":{"name":"Report Grid","story":"$33.story","table":"$34.table"}}),
        json!({"command":"style.cell.create","params":{"name":"Header Cell","fill":"Brand Navy","insets":6,"vj":"center"}}),
        json!({"command":"style.cell.apply","params":{"name":"Header Cell","story":"$33.story","table":"$34.table","rows":[0,0],"cols":[0,2]}}),
        json!({"command":"table.insertRow","params":{"story":"$33.story","table":"$34.table","row":1,"where":"below","count":1}}),
        json!({"command":"table.insertColumn","params":{"story":"$33.story","table":"$34.table","col":1,"where":"right","count":1,"width":100}}),
        json!({"command":"table.setRowHeight","params":{"story":"$33.story","table":"$34.table","row":0,"height":28,"mode":"atLeast"}}),
        json!({"command":"table.setColumnWidth","params":{"story":"$33.story","table":"$34.table","col":0,"width":140}}),
        json!({"command":"table.distributeColumns","params":{"story":"$33.story","table":"$34.table","cols":[0,3]}}),
        json!({"command":"table.distributeRows","params":{"story":"$33.story","table":"$34.table","rows":[0,3]}}),
        json!({"command":"table.merge","params":{"story":"$33.story","table":"$34.table","rows":[0,0],"cols":[0,1]}}),
        json!({"command":"table.unmerge","params":{"story":"$33.story","table":"$34.table","row":0,"col":0}}),
        json!({"command":"table.deleteRow","params":{"story":"$33.story","table":"$34.table","row":3}}),
        json!({"command":"table.deleteColumn","params":{"story":"$33.story","table":"$34.table","col":3}}),
        json!({"command":"table.options","params":{"story":"$33.story","table":"$34.table","direction":"rightToLeft","repeatHeader":true,"spaceBefore":6,"spaceAfter":8}}),
    ]);
    let full_plan = json!({"outputMode":"native_only","commands":cmds});
    s.execute("production.apply", &full_plan).unwrap();

    let doc = &s.doc().unwrap().doc;
    assert!(doc.styles.paragraph.iter().any(|st| st.name == "Arabic Body"));
    assert!(doc.styles.character.iter().any(|st| st.name == "Gold Highlight"));
    assert!(doc.styles.object.iter().any(|st| st.name == "Brand Card"));
    let report_grid = doc.styles.table.iter().find(|st| st.name == "Report Grid").unwrap();
    let alt_rows = report_grid.alt_rows.as_ref().unwrap();
    assert_eq!(alt_rows.first, 1);
    assert_eq!(alt_rows.next, 1);
    assert_eq!(alt_rows.next_color, "Brand Gold");
    assert!(doc.styles.cell.iter().any(|st| st.name == "Header Cell"));
    let grad_swatch = doc
        .swatches
        .iter()
        .find_map(|sw| match &sw.value {
            designcraft_doc::color::SwatchValue::Gradient { gradient } => Some(&gradient.stops),
            _ => None,
        })
        .expect("gradient swatch should be created");
    assert_eq!(grad_swatch.len(), 2);
    assert!((grad_swatch[0].offset - 0.0).abs() < 1e-4);
    assert!((grad_swatch[1].offset - 1.0).abs() < 1e-4);

    let before = s.doc().unwrap().doc.clone();
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    let step_repeat_graphic = json!({"outputMode":"native_with_images","commands":[
        {"command":"frame.create","params":{"rect":[54,54,154,154],"content":"graphic"}},
        {"command":"file.place","params":{"frame":"$0.id","name":"illustration.png","base64":png}},
        {"command":"edit.stepAndRepeat","params":{"ids":["$0.id"],"count":2,"dx":40,"dy":0}}
    ]});
    assert!(s.execute("production.apply", &step_repeat_graphic).is_err());

    let bad_qr_rect = json!({"outputMode":"native_only","commands":[
        {"command":"object.qrCode","params":{"type":"url","content":"https://example.com","rect":[200,200,100,100]}}
    ]});
    assert!(s.execute("production.apply", &bad_qr_rect).is_err());
    let bad_qr_swatch = json!({"outputMode":"native_only","commands":[
        {"command":"object.qrCode","params":{"type":"url","content":"https://example.com","color":"NonExistentSwatch"}}
    ]});
    assert!(s.execute("production.apply", &bad_qr_swatch).is_err());
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
}

#[test]
fn file_new_preserves_right_to_left_binding_even_without_facing_pages() {
    let mut s = Session::new();
    s.execute("file.new", &json!({"width":595.28,"height":841.89,"facingPages":false,"binding":"rightToLeft","pages":2})).unwrap();
    let doc = &s.doc().unwrap().doc;
    assert!(!doc.settings.facing_pages);
    assert!(doc.settings.right_to_left_binding);
}

#[test]
fn layout_guard_allows_background_cards_below_text_and_rejects_opaque_occluders_above_text() {
    let font = layout_font();
    let mut s = session();
    let valid_bg = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
        {"command":"frame.create","params":{"rect":[54,54,260,120],"content":"unassigned"}},
        {"command":"object.fill","params":{"ids":["$0.id"],"swatch":"[Paper]"}},
        {"command":"frame.create","params":{"rect":[62,62,260,110],"content":"text","text":"ABBA","caret":false}},
        {"command":"type.char","params":{"attrs":{"fontFamily":font}}},
        {"command":"line.create","params":{"a":[54,85],"b":[260,85]}}
    ]});
    s.execute("production.apply", &valid_bg).unwrap();

    let before = s.doc().unwrap().doc.clone();
    let occluding_rect = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
        {"command":"frame.create","params":{"rect":[60,60,240,108],"content":"unassigned"}},
        {"command":"object.fill","params":{"ids":["$0.id"],"swatch":"[Paper]"}}
    ]});
    let err = s.execute("production.apply", &occluding_rect).unwrap_err();
    assert!(err.to_string().contains("textOccludedByObject"), "expected textOccludedByObject, got: {err}");
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));

    let bg_id = s.doc().unwrap().doc.spreads[0].items[0].id.0;
    let bring_bg_to_front = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
        {"command":"object.arrange","params":{"ids":[bg_id],"to":"front"}}
    ]});
    let err_arrange = s.execute("production.apply", &bring_bg_to_front).unwrap_err();
    assert!(err_arrange.to_string().contains("textOccludedByObject"), "expected textOccludedByObject on arrange, got: {err_arrange}");
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));

    let transparent_png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mNgAAIAAAUAAen63NgAAAAASUVORK5CYII=";
    let transparent_overlay = json!({"outputMode":"native_with_images","requirePreflight":true,"minPpi":1,"commands":[
        {"command":"frame.create","params":{"rect":[58,58,255,112],"content":"graphic"}},
        {"command":"file.place","params":{"frame":"$0.id","name":"overlay-clear.png","base64":transparent_png}},
        {"command":"object.fit","params":{"ids":["$0.id"],"mode":"fitContentToFrame"}}
    ]});
    s.execute("production.apply", &transparent_overlay).unwrap();

    let sparse_png = "iVBORw0KGgoAAAANSUhEUgAAAAoAAAABCAYAAADn9T9+AAAAAXNSR0IArs4c6QAAAARnQU1BAACxjwv8YQUAAAAJcEhZcwAADsMAAA7DAcdvqGQAAAAQSURBVBhXY2AgAoiIiPwHAAHcATwojoEhAAAAAElFTkSuQmCC";
    let sparse_overlay = json!({"outputMode":"native_with_images","requirePreflight":true,"minPpi":1,"commands":[
        {"command":"frame.create","params":{"rect":[58,58,255,112],"content":"graphic"}},
        {"command":"file.place","params":{"frame":"$0.id","name":"overlay-sparse.png","base64":sparse_png}},
        {"command":"object.fit","params":{"ids":["$0.id"],"mode":"fitContentToFrame"}}
    ]});
    s.execute("production.apply", &sparse_overlay).unwrap();

    let after_sparse = s.doc().unwrap().doc.clone();
    let dense_png = "iVBORw0KGgoAAAANSUhEUgAAAAoAAAABCAYAAADn9T9+AAAAAXNSR0IArs4c6QAAAARnQU1BAACxjwv8YQUAAAAJcEhZcwAADsMAAA7DAcdvqGQAAAARSURBVBhXYxAREflPCDMAAQDs8AsUSDJVGgAAAABJRU5ErkJggg==";
    let dense_overlay = json!({"outputMode":"native_with_images","requirePreflight":true,"minPpi":1,"commands":[
        {"command":"frame.create","params":{"rect":[58,58,255,112],"content":"graphic"}},
        {"command":"file.place","params":{"frame":"$0.id","name":"overlay-dense.png","base64":dense_png}},
        {"command":"object.fit","params":{"ids":["$0.id"],"mode":"fitContentToFrame"}}
    ]});
    let err_dense_png = s.execute("production.apply", &dense_overlay).unwrap_err();
    assert!(err_dense_png.to_string().contains("textOccludedByObject"), "expected textOccludedByObject on dense PNG, got: {err_dense_png}");
    assert!(Arc::ptr_eq(&after_sparse, &s.doc().unwrap().doc));

    let opaque_png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAAXNSR0IArs4c6QAAAARnQU1BAACxjwv8YQUAAAAJcEhZcwAADsMAAA7DAcdvqGQAAAANSURBVBhXY/jPwPAfAAUAAf+mXJtdAAAAAElFTkSuQmCC";
    let opaque_overlay = json!({"outputMode":"native_with_images","requirePreflight":true,"minPpi":1,"commands":[
        {"command":"frame.create","params":{"rect":[58,58,255,112],"content":"graphic"}},
        {"command":"file.place","params":{"frame":"$0.id","name":"overlay-solid.png","base64":opaque_png}},
        {"command":"object.fit","params":{"ids":["$0.id"],"mode":"fitContentToFrame"}}
    ]});
    let err_opaque_png = s.execute("production.apply", &opaque_overlay).unwrap_err();
    assert!(err_opaque_png.to_string().contains("textOccludedByObject"), "expected textOccludedByObject on opaque PNG, got: {err_opaque_png}");
    assert!(Arc::ptr_eq(&after_sparse, &s.doc().unwrap().doc));
}

#[test]
fn serde_path_to_error_and_indexed_schema_paths_pinpoint_malformed_fields_and_support_oval_and_autosize() {
    let mut s = session();
    let before = s.doc().unwrap().doc.clone();

    let bad_envelope = json!({
        "outputMode": "native_only",
        "commands": [
            {"command": "frame.create", "params": {"rect": [36, 36, 100, 100]}},
            {"command": "frame.create", "params": [1, 2, 3]}
        ]
    });
    let err_env = s.execute("production.validate", &bad_envelope).unwrap_err().to_string();
    assert!(err_env.contains("commands[1].params"), "expected commands[1].params in error: {err_env}");

    let bad_rect = json!({
        "outputMode": "native_only",
        "commands": [
            {"command": "frame.create", "params": {"rect": [36, 36, 100, 100]}},
            {"command": "frame.create", "params": {"rect": [36, 36, "bad", 100]}}
        ]
    });
    let err_rect = s.execute("production.validate", &bad_rect).unwrap_err().to_string();
    assert!(err_rect.contains("commands[1].params.rect[2]"), "expected commands[1].params.rect[2] in error: {err_rect}");

    let bad_group = json!({
        "outputMode": "native_only",
        "chunks": [
            {"commands": [{"command": "frame.create", "params": {"rect": [36, 36, 100, 100]}}]},
            {"commands": [{"command": "frame.create", "params": {"rect": [36, "bad", 100, 100]}}]}
        ]
    });
    let err_grp = s.execute("production.applyGroup", &bad_group).unwrap_err().to_string();
    assert!(err_grp.contains("chunks[1].commands[0].params.rect[1]"), "expected chunks[1].commands[0].params.rect[1], got: {err_grp}");
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));

    let valid = json!({
        "outputMode": "native_only",
        "requirePreflight": true,
        "commands": [
            {"command": "frame.create", "params": {"rect": [36, 36, 120, 120], "shape": "oval", "content": "unassigned"}},
            {"command": "frame.create", "params": {"rect": [150, 36, 360, 52], "content": "text", "text": "Auto-sized text grows to avoid overset.\n".repeat(6), "caret": false}},
            {"command": "object.textFrameOptions", "params": {
                "ids": ["$1.id"],
                "autoSize": "heightOnly",
                "autoSizeReference": "topLeft",
                "useMinimumHeight": true,
                "minimumHeight": 40,
                "noLineBreaks": false
            }}
        ]
    });
    let r = s.execute("production.apply", &valid).unwrap();
    let doc = &s.doc().unwrap().doc;
    let oval_id = designcraft_doc::ItemId(r["results"][0]["id"].as_u64().unwrap());
    let tf_id = designcraft_doc::ItemId(r["results"][1]["id"].as_u64().unwrap());
    assert_eq!(doc.item(oval_id).unwrap().shape, designcraft_doc::Shape::Oval);
    assert!(doc.item(tf_id).unwrap().bounds().height() > 52.0 - 36.0 + 20.0, "autoSize heightOnly grew the frame");
}

#[test]
fn relational_parent_spread_running_headers_live_captions_and_grep_apply_in_one_undo() {
    let mut s = Session::new();
    s.execute("file.new", &json!({"pages": 3, "facingPages": true, "binding": "rightToLeft", "width": 595.28, "height": 841.89})).unwrap();
    let before = s.doc().unwrap().doc.clone();
    let undo_before = s.doc().unwrap().history.undo.len();

    let plan = json!({
        "outputMode": "native_only",
        "label": "Relational RTL Layout",
        "commands": [
            {"command": "style.paragraph.create", "params": {"name": "Unit Heading", "para": {"align": "right", "direction": "rightToLeft"}, "chars": {"size": 18, "fontStyle": "Bold"}}},
            {"command": "variables.define", "params": {"name": "Unit Running", "type": "runningHeader", "style": "Unit Heading"}},
            {"command": "layout.parents.new", "params": {"prefix": "U", "name": "Unit Master"}},
            {"command": "layout.parents.options", "params": {"parent": "$2.id", "basedOn": null}},
            {"command": "frame.create", "params": {"spread": {"kind": "parent", "index": "$2.index"}, "rect": [48, 780, 120, 804], "content": "text", "text": "\u{E000}", "caret": false}},
            {"command": "frame.create", "params": {"spread": {"kind": "parent", "index": "$2.index"}, "rect": [1070, 780, 1142, 804], "content": "text", "text": "\u{E000}", "caret": false}},
            {"command": "frame.create", "params": {"spread": {"kind": "parent", "index": "$2.index"}, "rect": [750, 36, 1142, 62], "content": "text", "text": "", "caret": false}},
            {"command": "text.select", "params": {"story": "$6.story", "anchor": 0, "focus": 0}},
            {"command": "variables.insert", "params": {"name": "Unit Running"}},
            {"command": "layout.pages.applyParent", "params": {"pages": [1, 2], "parent": "$2.id"}},
            {"command": "layout.section", "params": {"page": 1, "startNumber": 1, "style": "arabic", "marker": "الوحدة ١"}},
            {"command": "layout.chapterNumbering", "params": {"start": 1, "source": "userDefined", "style": "arabic"}},
            {"command": "frame.create", "params": {"spread": 1, "rect": [700, 90, 1142, 130], "content": "text", "text": "الوحدة الأولى: الأخلاق", "caret": false}},
            {"command": "text.select", "params": {"story": "$12.story", "anchor": 0, "focus": 10}},
            {"command": "style.paragraph.apply", "params": {"name": "Unit Heading"}},
            {"command": "frame.create", "params": {"spread": 1, "rect": [960, 180, 1120, 340], "content": "graphic"}},
            {"command": "object.altText", "params": {"ids": ["$15.id"], "text": "مسجد الحي"}},
            {"command": "object.caption", "params": {"ids": ["$15.id"], "text": "{altText}", "position": "below", "offset": 6, "height": 24, "live": true, "group": true}},
            {"command": "find.change", "params": {"find": "الأخلاق", "change": "الآداب الإسلامية", "grep": true, "scope": "document", "attrs": {"underline": true}}}
        ]
    });

    let res = s.execute("production.apply", &plan).unwrap();
    assert_eq!(res["steps"], 19);
    assert_eq!(s.doc().unwrap().history.undo.len(), undo_before + 1);

    let inspected = s.execute("document.inspect", &json!({})).unwrap();
    assert_eq!(inspected["settings"]["rightToLeftBinding"], true);
    assert!(inspected["parents"].as_array().unwrap().iter().any(|p| p["label"] == "U-Unit Master" && p["items"] == 3));
    let gf_id = designcraft_doc::ItemId(res["results"][15]["id"].as_u64().unwrap());
    let cap_id = designcraft_doc::ItemId(res["results"][17]["ids"][0].as_u64().unwrap());
    let grp_id = designcraft_doc::ItemId(res["results"][17]["groups"][0].as_u64().unwrap());
    assert_eq!(s.doc().unwrap().doc.find(gf_id).unwrap().path.len(), 2, "graphic frame grouped with its live caption");
    assert_eq!(s.doc().unwrap().doc.find(cap_id).unwrap().path.len(), 2, "caption frame grouped with its graphic");
    assert!(s.doc().unwrap().doc.item(grp_id).unwrap().is_group());

    let heading_sid = designcraft_doc::StoryId(res["results"][12]["story"].as_u64().unwrap());
    assert_eq!(s.doc().unwrap().doc.story(heading_sid).unwrap().text, "الوحدة الأولى: الآداب الإسلامية");

    // Live caption updates automatically when altText changes on the linked graphic frame inside the group:
    s.execute("object.altText", &json!({"ids": [gf_id.0], "text": "بيت الله"})).unwrap();
    let cap_sid = s.doc().unwrap().doc.item(cap_id).unwrap().text_frame().unwrap().story;
    assert_eq!(s.doc().unwrap().doc.story(cap_sid).unwrap().text, "بيت الله");

    // Undo altText + undo relational production plan restores original document atomically:
    s.execute("edit.undo", &json!({})).unwrap();
    s.execute("edit.undo", &json!({})).unwrap();
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
}

#[test]
fn unified_command_surface_matches_params_source_of_truth_and_executes_derived_commands() {
    let all = schemas();
    assert!(all.len() >= 140, "expected unified safe engine command surface (>=140 commands), got {}", all.len());

    // Every parameter property in every production schema must be accepted by `cmd::params::check_params`!
    let mut drifts = Vec::new();
    for (id, schema, _) in &all {
        assert_eq!(schema["additionalProperties"], json!(false), "`{id}` must forbid unknown properties");
        let props = schema["properties"].as_object().unwrap_or_else(|| panic!("`{id}` missing properties"));
        for key in props.keys() {
            let probe = json!({ key: Value::Null });
            if let Err(e) = super::super::params::check_params(id, &probe) {
                drifts.push(format!("`{id}`.`{key}` -> {e}"));
            }
        }
        if *id != "file.place" {
            for forbidden in ["path", "paths", "file", "files", "folder", "dir", "directory", "base64", "dataBase64"] {
                assert!(!props.contains_key(forbidden), "`{id}` must never expose IO/binary parameter `{forbidden}`");
            }
        }
    }
    assert!(drifts.is_empty(), "schema drift between production::schemas and cmd::params::check_params:\n{}", drifts.join("\n"));

    // Full catalog allowlist (over 140 commands) works seamlessly with `allowedCommands`:
    let full_allowlist: Vec<String> = all.iter().map(|(id, _, _)| (*id).to_string()).collect();
    let mut s = session();
    let before = s.doc().unwrap().doc.clone();
    let plan = json!({
        "outputMode": "native_only",
        "allowedCommands": full_allowlist,
        "commands": [
            {"command": "style.compositeFont.set", "params": {
                "name": "KG Bilingual",
                "entries": [
                    {"characters": "", "family": "Arial"},
                    {"characters": "0123456789", "family": "Arial", "relativeSize": 95.0}
                ]
            }},
            {"command": "layer.new", "params": {"name": "Artwork"}},
            {"command": "frame.create", "params": {"rect": [72, 72, 260, 140], "content": "text", "text": "hello world", "caret": false}},
            {"command": "text.select", "params": {"story": "$2.story", "anchor": 0, "focus": 11}},
            {"command": "type.changeCase", "params": {"case": "upper"}},
            {"command": "frame.create", "params": {"rect": [300, 72, 380, 152], "content": "unassigned"}},
            {"command": "object.convertShape", "params": {"ids": ["$5.id"], "to": "ellipse"}}
        ]
    });
    s.execute("production.apply", &plan).unwrap();
    let d = &s.doc().unwrap().doc;
    assert!(d.styles.composite_fonts.iter().any(|f| f.name == "KG Bilingual"));
    assert!(d.layers.iter().any(|l| l.name == "Artwork"));
    let sid = designcraft_doc::StoryId(d.spreads[0].items[0].text_frame().unwrap().story.0);
    assert_eq!(d.story(sid).unwrap().text, "HELLO WORLD");
    assert_eq!(d.spreads[0].items[1].shape, designcraft_doc::Shape::Oval);

    s.execute("edit.undo", &json!({})).unwrap();
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
}

#[test]
fn apply_group_defers_preflight_until_all_repair_chunks_complete() {
    let mut s = Session::new();
    s.execute("file.new", &json!({})).unwrap();
    let f = s.execute("frame.create", &json!({"rect": [72, 72, 180, 102], "content": "text", "text": "Short", "caret": false})).unwrap();
    let sid = f["story"].as_u64().unwrap();

    // Chunk 0 creates temporary overset; Chunk 1 fits the overset story. Final draft passes preflight.
    let repaired = json!({
        "outputMode": "native_only",
        "requirePreflight": true,
        "chunks": [
            {"commands": [{"command": "story.setText", "params": {"story": sid, "text": "Long overflowing sentence that does not fit at default 12pt inside a narrow 30pt-tall box."}}]},
            {"commands": [{"command": "story.fitOverset", "params": {"story": sid, "minSize": 6.5}}]}
        ]
    });
    let res = s.execute("production.applyGroup", &repaired).unwrap();
    assert_eq!(res["groups"], 2);

    // Unrepaired overset at the end of the group is still rejected by final preflight.
    let unrepaired = json!({
        "outputMode": "native_only",
        "requirePreflight": true,
        "chunks": [
            {"commands": [{"command": "story.setText", "params": {"story": sid, "text": "Overflowing words ".repeat(60)}}]}
        ]
    });
    let err = s.execute("production.applyGroup", &unrepaired).unwrap_err().to_string();
    assert!(err.contains("preflight rejected draft"), "got {err}");
}

#[test]
fn liquid_layout_alternate_layouts_reuse_asset_and_headless_variables_execute_in_production() {
    let mut s = Session::new();
    s.execute("file.new", &json!({"width": 600.0, "height": 800.0, "pages": 1, "facingPages": false, "binding": "rightToLeft"})).unwrap();
    let before = s.doc().unwrap().doc.clone();
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    let plan = json!({
        "outputMode": "native_with_images",
        "label": "Multi-Format Campaign",
        "commands": [
            {"command": "liquid.pageRule", "params": {"rule": "objectBased", "pages": [1]}},
            {"command": "variables.define", "params": {"name": "CampaignTag", "type": "custom", "text": "إصدار ٢٠٢٦"}},
            {"command": "frame.create", "params": {"rect": [54, 54, 546, 120], "content": "text", "text": "حملة التعليم — ", "caret": false}},
            {"command": "variables.insert", "params": {"name": "CampaignTag", "frame": "$2.id"}},
            {"command": "liquid.object", "params": {"ids": ["$2.id"], "resizeWidth": true, "resizeHeight": false, "pinTop": true, "pinLeft": true, "pinRight": true}},
            {"command": "file.place", "params": {"base64": png, "name": "badge.png", "x": 54, "y": 160, "width": 120}},
            {"command": "file.place", "params": {"reuseAsset": "$5.asset", "x": 220, "y": 160, "width": 120}},
            {"command": "file.place", "params": {"reuseAsset": "$5.asset", "x": 386, "y": 160, "width": 120}},
            {"command": "layout.createAlternate", "params": {"name": "Social Square", "width": 600.0, "height": 600.0, "rule": "objectBased"}}
        ]
    });

    let res = s.execute("production.apply", &plan).unwrap();
    assert_eq!(res["steps"], 9);
    let doc = &s.doc().unwrap().doc;
    assert_eq!(doc.assets.len(), 1, "reuseAsset must deduplicate to a single Asset entry in doc.assets");
    assert_eq!(res["results"][5]["asset"], res["results"][6]["asset"]);
    assert_eq!(res["results"][5]["asset"], res["results"][7]["asset"]);
    assert_eq!(doc.page_count(), 2, "layout.createAlternate adds the alternate page");
    let (_, _, alt_page) = doc.pages().nth(1).unwrap();
    assert!((alt_page.bounds().height() - 600.0).abs() < 1e-3);

    // Reject missing reuseAsset, simultaneous base64 + reuseAsset, or reuseAsset in native_only mode:
    let snapshot = s.doc().unwrap().doc.clone();
    let bad_missing = json!({
        "outputMode": "native_with_images",
        "commands": [{"command": "file.place", "params": {"reuseAsset": 999999, "x": 54, "y": 54, "width": 80}}]
    });
    assert!(s.execute("production.apply", &bad_missing).is_err());
    let existing_aid = res["results"][5]["asset"].as_u64().unwrap();
    let bad_both = json!({
        "outputMode": "native_with_images",
        "commands": [{"command": "file.place", "params": {"base64": png, "reuseAsset": existing_aid, "name": "x.png", "x": 54, "y": 54, "width": 80}}]
    });
    assert!(s.execute("production.apply", &bad_both).is_err());
    let bad_native_only = json!({
        "outputMode": "native_only",
        "commands": [{"command": "file.place", "params": {"reuseAsset": existing_aid, "x": 54, "y": 54, "width": 80}}]
    });
    assert!(s.execute("production.apply", &bad_native_only).is_err());
    assert!(Arc::ptr_eq(&snapshot, &s.doc().unwrap().doc));

    s.execute("edit.undo", &json!({})).unwrap();
    assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
}
