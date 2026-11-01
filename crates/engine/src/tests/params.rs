//! The params descriptions name the keys their commands accept (#776).

use serde_json::json;

use crate::cmd::params::{UNCHECKED, accepted_keys, check_keys, check_params, command_params_json_schema, doc_keys, parse_params_doc};
use crate::cmd::production::command_schema;
use crate::{EngineError, Session, command_specs, find_command};

fn keys(doc: &str) -> Vec<&str> {
    doc_keys(doc).unwrap().keys
}

#[test]
fn a_description_names_its_keys() {
    assert_eq!(keys("{} → {id}"), Vec::<&str>::new());
    assert_eq!(
        keys("{x, y, width?: pt=612|\"210 mm\", sides=6, name?|title?: a name (default: Untitled-N)} → {index}"),
        ["x", "y", "width", "sides", "name", "title"]
    );
    // Brackets, quotes and backticks keep their commas and bars; a nested object isn't listed.
    assert_eq!(
        keys("{at: [x, y] (page, not view), stops: [{position: pt, leader?: \", \"}], csv?: a `|, ` in a label is a break, on?: bool}"),
        ["at", "stops", "csv", "on"]
    );
    // Alternatives after a note are keys when they have their own `?` or `:`; else values.
    assert_eq!(
        keys("{steps?: n | distance?: pt | smooth?: bool, spacing?: smooth|steps|distance, path? | data?: text | dataBase64?}"),
        ["steps", "distance", "smooth", "spacing", "path", "data", "dataBase64"]
    );
    assert_eq!(keys("{latex, size?} or {id, latex}"), ["latex", "size", "id", "latex"]);
    let more = doc_keys("{name, …same as style.cell.create, …the format's options}").unwrap();
    assert_eq!((more.keys, more.more), (vec!["name"], vec!["same as style.cell.create", "the format's options"]));
    for bad in ["same as paint.setFill", "{a, b", "{opacity? 0..1}", "{path | name+dataBase64}", "{xRelative?/yRelative?: anchor}"] {
        assert!(doc_keys(bad).is_err(), "{bad}");
    }
}

/// Every command's description reads as a params object, so none is left unchecked by accident.
#[test]
fn every_description_parses() {
    let bad: Vec<String> = command_specs().iter().filter_map(|c| doc_keys(c.params).err().map(|e| format!("{}: {e}", c.id))).collect();
    assert!(bad.is_empty(), "{} descriptions don't parse:\n{}", bad.len(), bad.join("\n"));
    let stale: Vec<_> = UNCHECKED.iter().filter(|id| find_command(id).is_none()).collect();
    assert!(stale.is_empty() && UNCHECKED.is_sorted(), "UNCHECKED: sorted ids of commands; not commands: {stale:?}");
}

fn refusal(id: &str, params: serde_json::Value) -> String {
    match check_params(id, &params) {
        Err(EngineError::BadParams { cmd, msg }) if cmd == id => msg,
        other => panic!("{id} {params}: {other:?}"),
    }
}

/// The examples of #776: both ran and did nothing (or the default) without a word.
#[test]
fn the_issue_examples_are_refused() {
    // (The keys are checked in order: `end` comes before `start`.)
    assert_eq!(refusal("text.select", json!({"story": 1, "start": 0, "end": 5})), "unknown parameter `end`; the command takes: story, anchor, focus");
    assert_eq!(refusal("text.select", json!({"story": 1, "start": 0})), "unknown parameter `start`; the command takes: story, anchor, focus");
    assert_eq!(refusal("style.list", json!({"bogus": 1})), "unknown parameter `bogus`; the command takes no parameters");
}

#[test]
fn an_unknown_key_is_named_with_the_nearest_accepted_one() {
    assert_eq!(refusal("file.new", json!({"widht": 300})), "unknown parameter `widht`; did you mean `width`?");
    assert_eq!(refusal("file.new", json!({"Width": 300})), "unknown parameter `Width`; did you mean `width`?");
    assert_eq!(
        refusal("line.create", json!({"a": [0, 0], "b": [5, 5], "colour": "red"})),
        "unknown parameter `colour`; the command takes: spread, a, b"
    );
    for (id, ok) in [
        ("file.new", json!({"width": 300, "title": "a", "margins": {"anything": 1}})),
        ("file.new", json!({})),
        ("file.new", json!(null)),
        // A command that acts on `ids` takes one object as `id` too.
        ("transform.move", json!({"dx": 1, "dy": 1, "id": 3})),
        // Keys listed by reference.
        ("style.cell.edit", json!({"name": "a", "fill": "Paper"})),
        // On UNCHECKED: the attributes may be the params object itself.
        ("type.char", json!({"size": 12})),
        // Not a command: running it says so.
        ("no.such", json!({"x": 1})),
    ] {
        assert!(check_params(id, &ok).is_ok(), "{id} {ok}");
    }
    assert!(check_keys("x", "{a, …same as file.new}", &json!({"a": 1, "width": 2})).is_ok());
    assert!(check_keys("x", "{a, …same as file.new}", &json!({"b": 1})).is_err());
    // A `…` naming no command stands for any key.
    assert!(check_keys("x", "{a, …the format's options}", &json!({"b": 1})).is_ok());
}

/// [`Session::execute`] itself takes what it is given (old journals replay); only callers from
/// outside (control channel, MCP, CLI) are checked. In this crate's tests it checks too, as the
/// audit of the descriptions, so this goes through a journal entry instead.
#[test]
fn journal_entries_pass_the_check() {
    let mut s = Session::new();
    s.execute("file.new", &json!({})).unwrap();
    let f = s.execute("frame.create", &json!({"rect": [36, 36, 300, 200], "content": "text", "text": "Hello world"})).unwrap();
    s.execute("text.select", &json!({"story": f["story"], "anchor": 0, "focus": 5})).unwrap();
    s.execute("type.char", &json!({"attrs": {"size": 24}})).unwrap();
    s.execute("transform.move", &json!({"dx": 10, "dy": 0, "ids": [f["id"]]})).unwrap();
    assert!(s.journal.len() >= 4);
    for (id, params) in &s.journal {
        assert!(check_params(id, params).is_ok(), "{id} {params}");
    }
}

/// Convert Rows acts on the shared table target, so a call that names its table and cells isn't refused.
#[test]
fn convert_rows_takes_the_table_target() {
    for id in ["table.convertRowsToHeader", "table.convertRowsToBody", "table.convertRowsToFooter"] {
        check_params(id, &json!({"story": 1, "table": 0, "rows": [0, 1], "cols": [0, 0]})).unwrap();
        check_params(id, &json!({"row": 0, "col": 0})).unwrap();
    }
}

#[test]
fn structured_params_schema_derives_types_enums_and_validates_in_production() {
    let mut rich_derived_count = 0usize;
    for spec in command_specs() {
        let fields = parse_params_doc(spec.params).unwrap_or_else(|e| panic!("{}: {e}", spec.id));
        if let Some(accepted) = accepted_keys(spec) {
            let schema = command_params_json_schema(spec).unwrap_or_else(|| panic!("{} accepted_keys succeeded but schema was None", spec.id));
            let props = schema["properties"].as_object().expect("properties object");
            for key in &accepted {
                assert!(props.contains_key(*key), "{} schema missing accepted key `{key}`", spec.id);
            }
            if props.values().any(|v| v.get("type").is_some() || v.get("anyOf").is_some() || v.get("description").is_some()) {
                rich_derived_count += 1;
            }
        } else {
            let _ = fields;
        }
    }
    assert!(rich_derived_count >= 150, "expected >= 150 commands with structured derived schemas, got {rich_derived_count}");

    // Union `{…} or {…}` merges enum choices cleanly (`guide.add` orientation: horizontal|vertical|both):
    let guide_spec = find_command("guide.add").expect("guide.add exists");
    let guide_schema = command_params_json_schema(guide_spec).unwrap();
    assert_eq!(guide_schema["properties"]["orientation"]["enum"], json!(["horizontal", "vertical", "both"]));
    assert_eq!(guide_schema["properties"]["point"]["minItems"], json!(2));
    assert_eq!(guide_schema["properties"]["spreadGuide"]["type"], json!("boolean"));

    // Derived schema in `command_schema` / `production.validate` enforces real enums & booleans while allowing open hints:
    let caption_schema = command_schema("object.caption").unwrap();
    assert!(caption_schema["properties"].as_object().unwrap().contains_key("group"));
    let rulers_schema = command_schema("view.zeroPoint").unwrap();
    assert_eq!(rulers_schema["properties"]["reset"]["type"], json!("boolean"));
    assert_eq!(rulers_schema["properties"]["at"]["minItems"], json!(2));

    let mut s = Session::new();
    s.execute("file.new", &json!({})).unwrap();
    // Derived command in production.validate rejects invalid enum / boolean type on derived schema:
    let bad_guide = s.execute(
        "production.validate",
        &json!({
            "outputMode": "native_only",
            "commands": [{"command": "guide.add", "params": {"orientation": "diagonal", "position": 72}}]
        }),
    );
    assert!(bad_guide.is_err(), "derived enum schema should reject invalid orientation `diagonal`");
    let ok_guide = s
        .execute(
            "production.validate",
            &json!({
                "outputMode": "native_only",
                "commands": [
                    {"command": "guide.add", "params": {"orientation": "horizontal", "position": 72}},
                    {"command": "guide.add", "params": {"orientation": "both", "point": [72, 72], "spreadGuide": true}}
                ]
            }),
        )
        .unwrap();
    assert_eq!(ok_guide["valid"], json!(true));
}
