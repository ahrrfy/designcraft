//! Unified command parameter schemas for governed AI production and MCP inspection.
//!
//! High-frequency and security-critical commands carry rich curated schemas; every other safe,
//! non-UI, undoable document command in [`super::super::command_specs`] derives its bounded
//! parameter schema directly from [`super::super::params::accepted_keys`] so engine capabilities
//! never drift from `CommandSpec`.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use serde_json::{Value, json};

use super::super::{CommandSpec, bad};
use super::ID;
use crate::Result;

fn object(fields: &[(&str, Value)], required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": fields.iter().map(|(k, v)| (k.to_string(), v.clone())).collect::<serde_json::Map<_, _>>(),
        "required": required,
        "additionalProperties": false,
    })
}
fn number(min: f64, max: f64) -> Value {
    json!({"type": "number", "minimum": min, "maximum": max})
}
fn integer(min: u64, max: u64) -> Value {
    json!({"type": "integer", "minimum": min, "maximum": max})
}
fn string() -> Value {
    json!({"type": "string", "maxLength": 262144})
}
fn boolean() -> Value {
    json!({"type": "boolean"})
}
fn choice(values: &[&str]) -> Value {
    json!({"type": "string", "enum": values})
}
fn array(items: Value, min: usize, max: usize) -> Value {
    json!({"type": "array", "items": items, "minItems": min, "maxItems": max})
}
fn ids() -> Value {
    array(integer(1, u64::MAX), 1, 1024)
}
fn rect() -> Value {
    array(number(-15552.0, 15552.0), 4, 4)
}
fn point() -> Value {
    array(number(-15552.0, 15552.0), 2, 2)
}

fn char_attrs_schema() -> Value {
    let positive = || number(0.01, 1000.0);
    object(
        &[
            ("fontFamily", string()),
            ("fontStyle", string()),
            ("language", string()),
            ("size", positive()),
            ("fill", string()),
            ("tracking", number(-1000.0, 1000.0)),
            ("underline", boolean()),
            (
                "leading",
                json!({"anyOf":[object(&[("kind",choice(&["auto"]))], &["kind"]),object(&[("kind",choice(&["points"])),("value",positive())], &["kind","value"])]}),
            ),
        ],
        &[],
    )
}

fn para_attrs_schema() -> Value {
    object(
        &[
            ("align", choice(&["left", "center", "right", "leftJustified", "rightJustified", "centerJustified", "fullyJustified"])),
            ("direction", choice(&["leftToRight", "rightToLeft"])),
            ("leftIndent", number(0.0, 1000.0)),
            ("rightIndent", number(0.0, 1000.0)),
            ("firstLineIndent", number(-1000.0, 1000.0)),
            ("spaceBefore", number(0.0, 1000.0)),
            ("spaceAfter", number(0.0, 1000.0)),
            ("hyphenate", boolean()),
            ("listType", choice(&["none", "bullets", "numbers"])),
            ("listName", string()),
            ("startAt", integer(1, 9999)),
            ("bulletChar", string()),
            (
                "numberStyle",
                choice(&[
                    "arabic",
                    "upperRoman",
                    "lowerRoman",
                    "upperLetters",
                    "lowerLetters",
                    "arabicLeadingZero",
                    "arabicThreeDigits",
                    "arabicFourDigits",
                    "symbols",
                    "kanji",
                ]),
            ),
            ("numberExpression", string()),
            ("listSeparator", string()),
            ("kashidas", boolean()),
            ("dropCapLines", integer(0, 16)),
            ("dropCapChars", integer(0, 16)),
            ("shadingOn", boolean()),
            ("shadingColor", string()),
            ("shadingTint", number(0.0, 1.0)),
            ("borderOn", boolean()),
            ("borderColor", string()),
            ("borderTint", number(0.0, 1.0)),
        ],
        &[],
    )
}

fn drop_shadow_schema() -> Value {
    object(
        &[
            ("on", boolean()),
            ("distance", number(0.0, 1000.0)),
            ("angle", number(-360.0, 360.0)),
            ("opacity", number(0.0, 1.0)),
            ("size", number(0.0, 1000.0)),
            ("spread", number(0.0, 100.0)),
            ("color", string()),
        ],
        &[],
    )
}

fn inner_shadow_schema() -> Value {
    object(
        &[
            ("on", boolean()),
            ("distance", number(0.0, 1000.0)),
            ("angle", number(-360.0, 360.0)),
            ("opacity", number(0.0, 1.0)),
            ("size", number(0.0, 1000.0)),
            ("choke", number(0.0, 100.0)),
            ("color", string()),
        ],
        &[],
    )
}

fn outer_glow_schema() -> Value {
    object(
        &[("on", boolean()), ("color", string()), ("opacity", number(0.0, 1.0)), ("size", number(0.0, 1000.0)), ("spread", number(0.0, 100.0))],
        &[],
    )
}

fn inner_glow_schema() -> Value {
    object(
        &[
            ("on", boolean()),
            ("color", string()),
            ("opacity", number(0.0, 1.0)),
            ("size", number(0.0, 1000.0)),
            ("choke", number(0.0, 100.0)),
            ("center", boolean()),
        ],
        &[],
    )
}

fn bevel_schema() -> Value {
    object(
        &[
            ("on", boolean()),
            ("size", number(0.0, 1000.0)),
            ("depth", number(0.0, 1000.0)),
            ("angle", number(-360.0, 360.0)),
            ("globalLight", boolean()),
            ("highlight", string()),
            ("highlightOpacity", number(0.0, 1.0)),
            ("shadow", string()),
            ("shadowOpacity", number(0.0, 1.0)),
        ],
        &[],
    )
}

fn satin_schema() -> Value {
    object(
        &[
            ("on", boolean()),
            ("color", string()),
            ("opacity", number(0.0, 1.0)),
            ("angle", number(-360.0, 360.0)),
            ("distance", number(0.0, 1000.0)),
            ("size", number(0.0, 1000.0)),
            ("invert", boolean()),
        ],
        &[],
    )
}

fn directional_feather_schema() -> Value {
    object(&[("on", boolean()), ("widths", array(number(0.0, 1000.0), 4, 4))], &[])
}

fn gradient_feather_schema() -> Value {
    object(
        &[
            ("on", boolean()),
            ("radial", boolean()),
            ("angle", number(-360.0, 360.0)),
            ("start", number(0.0, 1.0)),
            ("end", number(0.0, 1.0)),
            ("from", point()),
            ("to", point()),
        ],
        &[],
    )
}

fn effects_schema() -> Value {
    object(
        &[
            ("dropShadow", drop_shadow_schema()),
            ("innerShadow", inner_shadow_schema()),
            ("outerGlow", outer_glow_schema()),
            ("innerGlow", inner_glow_schema()),
            ("bevel", bevel_schema()),
            ("satin", satin_schema()),
            ("feather", number(0.0, 1000.0)),
            ("directionalFeather", directional_feather_schema()),
            ("gradientFeather", gradient_feather_schema()),
        ],
        &[],
    )
}

fn cell_stroke_schema() -> Value {
    object(&[("weight", number(0.0, 1000.0)), ("color", string()), ("tint", number(0.0, 1.0))], &[])
}

fn alt_fills_schema() -> Value {
    object(
        &[
            ("first", integer(0, 100)),
            ("firstColor", string()),
            ("firstTint", number(0.0, 1.0)),
            ("next", integer(0, 100)),
            ("nextColor", string()),
            ("nextTint", number(0.0, 1.0)),
            ("skipFirst", integer(0, 100)),
            ("skipLast", integer(0, 100)),
        ],
        &[],
    )
}

fn color_value_schema() -> Value {
    json!({"anyOf":[
        json!({"type":"string","maxLength":64}),
        object(&[("c",number(0.0,100.0)),("m",number(0.0,100.0)),("y",number(0.0,100.0)),("k",number(0.0,100.0))], &["c","m","y","k"]),
        array(number(0.0, 255.0), 3, 3)
    ]})
}

fn composite_font_entry_schema() -> Value {
    object(
        &[
            ("name", string()),
            ("characters", string()),
            ("family", string()),
            ("style", string()),
            ("relativeSize", number(0.01, 100.0)),
            ("horizontalScale", number(0.01, 100.0)),
            ("verticalScale", number(0.01, 100.0)),
            ("baselineShift", number(-100.0, 100.0)),
            ("scaleOption", choice(&["center", "emBox"])),
        ],
        &["family"],
    )
}

fn curated_schemas() -> Vec<(&'static str, Value, Value)> {
    let dim = || number(0.01, 15552.0);
    let index = || integer(0, 999);
    let spread_target = || {
        json!({"anyOf":[
            integer(0, 999),
            object(&[("kind", choice(&["doc", "parent"])), ("index", integer(0, 999))], &["kind", "index"])
        ]})
    };
    let edges = || json!({"anyOf":[number(0.0,15552.0),array(number(0.0,15552.0),4,4)]});
    let blend_mode = || choice(&["normal", "multiply", "screen", "overlay", "darken", "lighten"]);
    vec![
        (
            "frame.create",
            object(
                &[
                    ("rect", rect()),
                    ("spread", spread_target()),
                    ("shape", choice(&["rectangle", "ellipse", "oval", "polygon"])),
                    ("content", choice(&["text", "graphic", "unassigned"])),
                    ("text", string()),
                    ("caret", boolean()),
                    ("vertical", boolean()),
                    ("sides", integer(3, 64)),
                    ("starInset", number(0.0, 100.0)),
                ],
                &["rect"],
            ),
            json!({"rect":[54,54,540,150],"content":"text","text":"عنوان الدرس","caret":false}),
        ),
        ("line.create", object(&[("a", point()), ("b", point()), ("spread", spread_target())], &["a", "b"]), json!({"a":[54,160],"b":[540,160]})),
        (
            "path.create",
            object(
                &[
                    ("spread", spread_target()),
                    ("closed", boolean()),
                    ("anchors", array(object(&[("p", point()), ("in", point()), ("out", point())], &["p"]), 2, 1024)),
                ],
                &["anchors"],
            ),
            json!({"anchors":[{"p":[54,54]},{"p":[88,54]},{"p":[71,78]}],"closed":true}),
        ),
        ("object.label", object(&[("ids", ids()), ("label", string())], &["ids", "label"]), json!({"ids":["$0.id"],"label":"lesson.heading"})),
        ("object.altText", object(&[("ids", ids()), ("text", string())], &["text"]), json!({"ids":["$0.id"],"text":"وصف البطاقة"})),
        (
            "object.caption",
            object(
                &[
                    ("ids", ids()),
                    ("text", string()),
                    ("position", choice(&["below", "above", "left", "right"])),
                    ("offset", number(-1000.0, 1000.0)),
                    ("height", number(1.0, 1000.0)),
                    ("style", string()),
                    ("live", boolean()),
                    ("group", boolean()),
                    ("captionFrame", integer(1, u64::MAX)),
                ],
                &[],
            ),
            json!({"ids":["$0.id"],"text":"{altText}","position":"below","offset":4,"height":22,"live":true,"group":true}),
        ),
        ("selection.set", object(&[("ids", array(integer(1, u64::MAX), 0, 1024)), ("add", boolean())], &["ids"]), json!({"ids":["$0.id"]})),
        (
            "object.fill",
            object(&[("ids", ids()), ("swatch", string()), ("tint", number(0.0, 1.0))], &["ids", "swatch"]),
            json!({"ids":["$0.id"],"swatch":"[Paper]"}),
        ),
        (
            "object.stroke",
            object(
                &[
                    ("ids", ids()),
                    ("swatch", string()),
                    ("weight", number(0.0, 1000.0)),
                    ("tint", number(0.0, 1.0)),
                    ("align", choice(&["center", "inside", "outside"])),
                    ("cap", choice(&["butt", "round", "projecting"])),
                    ("join", choice(&["miter", "round", "bevel"])),
                ],
                &["ids"],
            ),
            json!({"ids":["$0.id"],"swatch":"[Black]","weight":1}),
        ),
        (
            "object.arrange",
            object(&[("ids", ids()), ("to", choice(&["front", "forward", "backward", "back"]))], &["ids", "to"]),
            json!({"ids":["$0.id"],"to":"front"}),
        ),
        ("object.group", object(&[("ids", ids())], &["ids"]), json!({"ids":["$0.id","$1.id"]})),
        ("object.ungroup", object(&[("ids", ids())], &["ids"]), json!({"ids":["$0.id"]})),
        ("edit.clear", object(&[("ids", ids())], &["ids"]), json!({"ids":["$0.id"]})),
        (
            "object.pathfinder",
            object(&[("ids", ids()), ("op", choice(&["add", "subtract", "intersect", "exclude", "minusBack"]))], &["op"]),
            json!({"ids":["$0.id","$1.id"],"op":"subtract"}),
        ),
        ("object.makeCompoundPath", object(&[("ids", ids())], &[]), json!({"ids":["$0.id","$1.id"]})),
        ("object.releaseCompoundPath", object(&[("ids", ids())], &[]), json!({"ids":["$0.id"]})),
        (
            "edit.stepAndRepeat",
            object(
                &[
                    ("ids", ids()),
                    ("count", integer(1, 64)),
                    ("dx", number(-15552.0, 15552.0)),
                    ("dy", number(-15552.0, 15552.0)),
                    ("rows", integer(1, 32)),
                    ("columns", integer(1, 32)),
                ],
                &[],
            ),
            json!({"ids":["$0.id"],"count":3,"dx":0,"dy":48}),
        ),
        (
            "object.cornerOptions",
            object(
                &[
                    ("ids", ids()),
                    ("shape", choice(&["none", "rounded", "inverseRounded", "inset", "bevel", "fancy"])),
                    ("size", number(0.0, 1000.0)),
                ],
                &["ids", "shape", "size"],
            ),
            json!({"ids":["$0.id"],"shape":"rounded","size":12}),
        ),
        (
            "object.opacity",
            object(&[("ids", ids()), ("opacity", number(0.0, 1.0)), ("blend", blend_mode())], &["ids", "opacity"]),
            json!({"ids":["$0.id"],"opacity":0.85,"blend":"normal"}),
        ),
        (
            "object.dropShadow",
            object(
                &[
                    ("ids", ids()),
                    ("on", boolean()),
                    ("distance", number(0.0, 1000.0)),
                    ("angle", number(-360.0, 360.0)),
                    ("opacity", number(0.0, 1.0)),
                    ("size", number(0.0, 1000.0)),
                    ("spread", number(0.0, 100.0)),
                    ("color", string()),
                ],
                &["ids", "on"],
            ),
            json!({"ids":["$0.id"],"on":true,"distance":4,"angle":135,"opacity":0.25,"size":8}),
        ),
        ("object.feather", object(&[("ids", ids()), ("width", number(0.0, 1000.0))], &["width"]), json!({"ids":["$0.id"],"width":6})),
        (
            "object.gradientFeather",
            object(
                &[
                    ("ids", ids()),
                    ("on", boolean()),
                    ("radial", boolean()),
                    ("angle", number(-360.0, 360.0)),
                    ("start", number(0.0, 100.0)),
                    ("end", number(0.0, 100.0)),
                    ("from", point()),
                    ("to", point()),
                ],
                &[],
            ),
            json!({"ids":["$0.id"],"on":true,"angle":90,"start":10.0,"end":90.0}),
        ),
        (
            "object.effects",
            object(&[("ids", ids()), ("effects", effects_schema()), ("opacity", number(0.0, 1.0)), ("blend", blend_mode())], &[]),
            json!({"ids":["$0.id"],"effects":{"outerGlow":{"on":true,"color":"[Paper]","opacity":0.4,"size":10}}}),
        ),
        (
            "object.gradient",
            object(
                &[
                    ("ids", ids()),
                    ("kind", choice(&["linear", "radial"])),
                    (
                        "stops",
                        array(
                            object(
                                &[
                                    ("location", number(0.0, 100.0)),
                                    ("pos", number(0.0, 1.0)),
                                    ("color", color_value_schema()),
                                    ("swatch", string()),
                                    ("opacity", number(0.0, 100.0)),
                                    ("midpoint", number(13.0, 87.0)),
                                ],
                                &[],
                            ),
                            2,
                            16,
                        ),
                    ),
                    ("reverse", boolean()),
                    ("angle", number(-360.0, 360.0)),
                    ("from", point()),
                    ("to", point()),
                    ("target", choice(&["fill", "stroke"])),
                ],
                &[],
            ),
            json!({"ids":["$0.id"],"kind":"linear","angle":45,"stops":[{"location":0,"color":"#1E3A8A"},{"location":100,"color":"#3B82F6"}]}),
        ),
        (
            "object.textWrap",
            object(
                &[("ids", ids()), ("mode", choice(&["none", "boundingBox", "contour", "jumpObject", "jumpToNextColumn"])), ("offset", edges())],
                &["ids", "mode"],
            ),
            json!({"ids":["$0.id"],"mode":"boundingBox","offset":8}),
        ),
        (
            "object.align",
            object(
                &[
                    ("ids", ids()),
                    ("edge", choice(&["left", "hcenter", "right", "top", "vcenter", "bottom"])),
                    ("to", choice(&["selection", "margins", "page", "spread"])),
                ],
                &["ids", "edge"],
            ),
            json!({"ids":["$0.id"],"edge":"hcenter","to":"margins"}),
        ),
        (
            "object.distribute",
            object(
                &[
                    ("ids", ids()),
                    ("axis", choice(&["horizontal", "vertical"])),
                    ("by", choice(&["centers", "spacing"])),
                    ("spacing", number(0.0, 15552.0)),
                ],
                &["ids", "axis"],
            ),
            json!({"ids":["$0.id","$1.id","$2.id"],"axis":"vertical","by":"spacing","spacing":12}),
        ),
        (
            "object.fittingOptions",
            object(
                &[("ids", ids()), ("fitting", choice(&["fitProportionally", "fillProportionally"])), ("align", integer(0, 8)), ("crop", rect())],
                &["ids", "fitting"],
            ),
            json!({"ids":["$0.id"],"fitting":"fillProportionally","align":4,"crop":[0,0,0,0]}),
        ),
        (
            "object.fit",
            object(
                &[
                    ("ids", ids()),
                    ("mode", choice(&["fillProportionally", "fitProportionally", "fitContentToFrame", "centerContent", "fitFrameToContent"])),
                    ("axis", choice(&["both", "height", "width"])),
                ],
                &["mode"],
            ),
            json!({"ids":["$0.id"],"mode":"fitFrameToContent","axis":"height"}),
        ),
        (
            "object.textFrameOptions",
            object(
                &[
                    ("ids", ids()),
                    ("columns", integer(1, 64)),
                    ("gutter", number(0.0, 1000.0)),
                    ("inset", edges()),
                    ("verticalJustification", choice(&["top", "center", "bottom", "justify"])),
                    ("autoSize", choice(&["off", "heightOnly", "widthOnly", "heightAndWidth", "heightAndWidthProportionally"])),
                    ("autoSizeReference", choice(&crate::autosize::REFERENCES)),
                    ("useMinimumHeight", boolean()),
                    ("minimumHeight", number(0.0, crate::autosize::MAX_SIDE)),
                    ("useMinimumWidth", boolean()),
                    ("minimumWidth", number(0.0, crate::autosize::MAX_SIDE)),
                    ("noLineBreaks", boolean()),
                    ("ignoreWrap", boolean()),
                    ("balanceColumns", boolean()),
                ],
                &["ids"],
            ),
            json!({"ids":["$0.id"],"inset":6}),
        ),
        (
            "object.qrCode",
            object(
                &[
                    ("type", choice(&["url", "text", "sms", "email", "vcard"])),
                    ("content", string()),
                    ("number", string()),
                    ("message", string()),
                    ("to", string()),
                    ("subject", string()),
                    ("body", string()),
                    ("name", string()),
                    ("phone", string()),
                    ("email", string()),
                    ("org", string()),
                    ("url", string()),
                    ("color", string()),
                    ("rect", rect()),
                    ("spread", spread_target()),
                ],
                &[],
            ),
            json!({"type":"url","content":"https://example.com","rect":[450,700,540,790],"color":"[Black]"}),
        ),
        (
            "transform.move",
            object(
                &[("ids", ids()), ("dx", number(-15552.0, 15552.0)), ("dy", number(-15552.0, 15552.0)), ("copy", boolean())],
                &["ids", "dx", "dy"],
            ),
            json!({"ids":["$0.id"],"dx":0,"dy":20}),
        ),
        (
            "transform.set",
            object(
                &[
                    ("ids", ids()),
                    ("x", number(-15552.0, 15552.0)),
                    ("y", number(-15552.0, 15552.0)),
                    ("width", dim()),
                    ("height", dim()),
                    ("rotation", number(-360.0, 360.0)),
                    ("ref", integer(0, 8)),
                ],
                &["ids"],
            ),
            json!({"ids":["$0.id"],"x":54,"y":54,"ref":0}),
        ),
        (
            "transform.rotate",
            object(&[("ids", ids()), ("angle", number(-360.0, 360.0)), ("ref", integer(0, 8))], &["angle"]),
            json!({"ids":["$0.id"],"angle":15,"ref":4}),
        ),
        (
            "transform.scale",
            object(&[("ids", ids()), ("sx", number(0.01, 100.0)), ("sy", number(0.01, 100.0))], &["sx"]),
            json!({"ids":["$0.id"],"sx":1.1,"sy":1.1}),
        ),
        (
            "transform.flip",
            object(&[("ids", ids()), ("axis", choice(&["horizontal", "vertical"])), ("ref", integer(0, 8))], &["axis"]),
            json!({"ids":["$0.id"],"axis":"horizontal"}),
        ),
        (
            "text.select",
            object(
                &[("story", integer(1, u64::MAX)), ("anchor", integer(0, 16777216)), ("focus", integer(0, 16777216))],
                &["story", "anchor", "focus"],
            ),
            json!({"story":"$0.story","anchor":0,"focus":0}),
        ),
        ("text.insert", object(&[("text", string()), ("raw", boolean())], &["text"]), json!({"text":"نص قابل للتحرير"})),
        (
            "text.thread",
            object(
                &[
                    ("from", integer(1, u64::MAX)),
                    ("to", integer(1, u64::MAX)),
                    ("rect", rect()),
                    ("spread", spread_target()),
                    ("autoflow", boolean()),
                ],
                &[],
            ),
            json!({"from":"$0.id","to":"$1.id"}),
        ),
        (
            "text.unthread",
            object(&[("frame", integer(1, u64::MAX)), ("side", choice(&["after", "before"]))], &[]),
            json!({"frame":"$0.id","side":"after"}),
        ),
        (
            "story.setText",
            object(&[("story", integer(1, u64::MAX)), ("text", string())], &["story", "text"]),
            json!({"story":"$0.story","text":"نص الدرس"}),
        ),
        (
            "story.replaceRange",
            object(
                &[("story", integer(1, u64::MAX)), ("start", integer(0, 16777216)), ("end", integer(0, 16777216)), ("text", string())],
                &["story", "start", "end", "text"],
            ),
            json!({"story":"$0.story","start":0,"end":5,"text":"مقدمة"}),
        ),
        (
            "story.fitOverset",
            object(
                &[
                    ("pages", array(integer(1, 10000), 2, 2)),
                    ("scope", choice(&["document", "story"])),
                    ("story", integer(1, u64::MAX)),
                    ("stories", array(integer(1, u64::MAX), 1, 256)),
                    ("minSize", number(4.0, 72.0)),
                ],
                &[],
            ),
            json!({"pages":[1,32],"minSize":7.5}),
        ),
        (
            "story.setDirection",
            object(&[("story", integer(1, u64::MAX)), ("direction", choice(&["leftToRight", "rightToLeft"]))], &["story", "direction"]),
            json!({"story":"$0.story","direction":"rightToLeft"}),
        ),
        (
            "find.change",
            object(
                &[
                    ("find", json!({"type":"string","minLength":1,"maxLength":262144})),
                    ("change", string()),
                    ("grep", boolean()),
                    ("caseSensitive", boolean()),
                    ("wholeWord", boolean()),
                    ("scope", choice(&["document", "story"])),
                    ("story", integer(1, u64::MAX)),
                    ("first", boolean()),
                    ("attrs", char_attrs_schema()),
                ],
                &["find", "change"],
            ),
            json!({"find":"النص القديم","change":"النص الجديد","scope":"document"}),
        ),
        (
            "find.changeObjects",
            object(
                &[
                    ("fill", string()),
                    ("stroke", string()),
                    ("strokeWeight", number(0.0, 1000.0)),
                    ("opacity", number(0.0, 1.0)),
                    ("kind", choice(&["text", "graphic", "shape", "line", "group"])),
                    ("layer", string()),
                    ("label", string()),
                    ("select", boolean()),
                    (
                        "change",
                        object(
                            &[("fill", string()), ("stroke", string()), ("strokeWeight", number(0.0, 1000.0)), ("opacity", number(0.0, 1.0))],
                            &[],
                        ),
                    ),
                ],
                &["change"],
            ),
            json!({"kind":"shape","label":"badge","change":{"fill":"[Paper]"}}),
        ),
        ("type.char", object(&[("attrs", char_attrs_schema())], &["attrs"]), json!({"attrs":{"size":14,"fontFamily":"Arial"}})),
        ("type.para", object(&[("attrs", para_attrs_schema())], &["attrs"]), json!({"attrs":{"align":"right","direction":"rightToLeft"}})),
        (
            "list.define",
            object(&[("name", string()), ("continueAcrossStories", boolean()), ("rename", string())], &["name"]),
            json!({"name":"Lesson Steps","continueAcrossStories":true}),
        ),
        (
            "style.compositeFont.set",
            object(&[("name", string()), ("entries", array(composite_font_entry_schema(), 1, 256))], &["name", "entries"]),
            json!({"name":"Arabic+Latin","entries":[{"characters":"","family":"Noto Sans Arabic"},{"characters":"0123456789","family":"Inter"}]}),
        ),
        (
            "style.paragraph.create",
            object(
                &[
                    ("name", string()),
                    ("basedOn", string()),
                    ("nextStyle", string()),
                    ("para", para_attrs_schema()),
                    ("chars", char_attrs_schema()),
                    ("fromSelection", boolean()),
                ],
                &["name"],
            ),
            json!({"name":"Heading 1","para":{"align":"right","direction":"rightToLeft","spaceAfter":8},"chars":{"size":22,"fontStyle":"Bold"}}),
        ),
        (
            "style.paragraph.edit",
            object(
                &[
                    ("name", string()),
                    ("rename", string()),
                    ("basedOn", string()),
                    ("nextStyle", string()),
                    ("para", para_attrs_schema()),
                    ("chars", char_attrs_schema()),
                ],
                &["name"],
            ),
            json!({"name":"Heading 1","chars":{"size":24}}),
        ),
        (
            "style.paragraph.apply",
            object(&[("name", string()), ("clearOverrides", boolean())], &["name"]),
            json!({"name":"Heading 1","clearOverrides":true}),
        ),
        (
            "style.character.create",
            object(&[("name", string()), ("basedOn", string()), ("chars", char_attrs_schema()), ("fromSelection", boolean())], &["name"]),
            json!({"name":"Emphasis","chars":{"fontStyle":"Bold","fill":"[Black]"}}),
        ),
        (
            "style.character.edit",
            object(&[("name", string()), ("rename", string()), ("basedOn", string()), ("chars", char_attrs_schema())], &["name"]),
            json!({"name":"Emphasis","chars":{"underline":true}}),
        ),
        ("style.character.apply", object(&[("name", string())], &["name"]), json!({"name":"Emphasis"})),
        (
            "style.object.create",
            object(
                &[
                    ("name", string()),
                    ("basedOn", string()),
                    ("fromSelection", boolean()),
                    ("fill", string()),
                    ("paragraphStyle", string()),
                    ("effects", effects_schema()),
                    ("dropShadow", drop_shadow_schema()),
                    ("feather", number(0.0, 1000.0)),
                ],
                &["name"],
            ),
            json!({"name":"Callout Card","fill":"[Paper]","dropShadow":{"on":true,"distance":4,"angle":135,"opacity":0.2,"size":8}}),
        ),
        ("style.object.apply", object(&[("name", string()), ("ids", ids())], &["name"]), json!({"name":"Callout Card","ids":["$0.id"]})),
        (
            "style.table.create",
            object(
                &[
                    ("name", string()),
                    ("basedOn", string()),
                    ("header", string()),
                    ("body", string()),
                    ("footer", string()),
                    ("leftColumn", string()),
                    ("rightColumn", string()),
                    ("border", cell_stroke_schema()),
                    ("altRows", alt_fills_schema()),
                    ("spaceBefore", number(0.0, 1000.0)),
                    ("spaceAfter", number(0.0, 1000.0)),
                ],
                &["name"],
            ),
            json!({"name":"Schedule Table","border":{"weight":1,"color":"[Black]"},"spaceBefore":6,"spaceAfter":6}),
        ),
        (
            "style.table.apply",
            object(
                &[
                    ("name", string()),
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &["name"],
            ),
            json!({"name":"Schedule Table","story":"$0.story","table":"$1.table"}),
        ),
        (
            "style.cell.create",
            object(
                &[
                    ("name", string()),
                    ("fromSelection", boolean()),
                    ("fill", string()),
                    ("tint", number(0.0, 1.0)),
                    ("insets", edges()),
                    ("vj", choice(&["top", "center", "bottom", "justify"])),
                    ("firstBaseline", choice(&["ascent", "capHeight", "leading", "xHeight", "fixed"])),
                    ("firstBaselineMin", number(0.0, 1000.0)),
                    ("stroke", cell_stroke_schema()),
                    ("paragraphStyle", string()),
                    ("basedOn", string()),
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &["name"],
            ),
            json!({"name":"Header Cell","fill":"[Black]","insets":6,"vj":"center"}),
        ),
        (
            "style.cell.apply",
            object(
                &[
                    ("name", string()),
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &["name"],
            ),
            json!({"name":"Header Cell","story":"$0.story","table":"$1.table","rows":[0,0],"cols":[0,2]}),
        ),
        (
            "swatch.create",
            object(&[("name", string()), ("color", color_value_schema()), ("spot", boolean())], &["color"]),
            json!({"name":"School blue","color":{"c":100,"m":60,"y":0,"k":0}}),
        ),
        (
            "layout.documentSetup",
            object(
                &[
                    ("width", dim()),
                    ("height", dim()),
                    ("pages", integer(1, 1000)),
                    ("margins", number(0.0, 15552.0)),
                    ("bleed", edges()),
                    ("slug", edges()),
                    ("facingPages", boolean()),
                    ("binding", choice(&["leftToRight", "rightToLeft"])),
                    ("intent", choice(&["print", "web", "mobile"])),
                    ("adjustLayout", boolean()),
                ],
                &[],
            ),
            json!({"width":595.2756,"height":841.8898,"margins":54,"facingPages":false}),
        ),
        (
            "layout.pages.insert",
            object(
                &[
                    ("count", integer(1, 100)),
                    ("after", index()),
                    ("parent", json!({"anyOf":[integer(1, u64::MAX), string(), json!({"type":"null"})]})),
                ],
                &["count"],
            ),
            json!({"count":1}),
        ),
        ("layout.pages.delete", object(&[("pages", array(index(), 1, 1000))], &["pages"]), json!({"pages":[2]})),
        (
            "layout.parents.new",
            object(&[("prefix", json!({"type":"string","minLength":1,"maxLength":8})), ("name", string())], &[]),
            json!({"prefix":"B","name":"Lesson"}),
        ),
        (
            "layout.parents.options",
            object(
                &[
                    ("parent", json!({"anyOf":[integer(1, u64::MAX), string()]})),
                    ("basedOn", json!({"anyOf":[integer(1, u64::MAX), string(), json!({"type":"null"})]})),
                ],
                &["parent", "basedOn"],
            ),
            json!({"parent":"B","basedOn":null}),
        ),
        (
            "layout.pages.applyParent",
            object(
                &[("pages", array(index(), 1, 1000)), ("parent", json!({"anyOf":[integer(1, u64::MAX), string(), json!({"type":"null"})]}))],
                &["pages", "parent"],
            ),
            json!({"pages":[1,2],"parent":"B"}),
        ),
        (
            "layout.section",
            object(
                &[
                    ("page", integer(1, 10000)),
                    ("startNumber", json!({"anyOf":[integer(1, 99999), json!({"type":"null"})]})),
                    (
                        "style",
                        choice(&[
                            "arabic",
                            "upperRoman",
                            "lowerRoman",
                            "upperLetters",
                            "lowerLetters",
                            "arabicLeadingZero",
                            "arabicThreeDigits",
                            "arabicFourDigits",
                            "kanji",
                        ]),
                    ),
                    ("prefix", json!({"type":"string","maxLength":8})),
                    ("includePrefix", boolean()),
                    ("marker", string()),
                    ("remove", boolean()),
                ],
                &["page"],
            ),
            json!({"page":1,"startNumber":1,"style":"arabic","marker":"الوحدة ١"}),
        ),
        (
            "layout.chapterNumbering",
            object(
                &[
                    (
                        "style",
                        choice(&[
                            "arabic",
                            "upperRoman",
                            "lowerRoman",
                            "upperLetters",
                            "lowerLetters",
                            "arabicLeadingZero",
                            "arabicThreeDigits",
                            "arabicFourDigits",
                        ]),
                    ),
                    ("start", integer(1, 99999)),
                    ("source", choice(&["automatic", "userDefined", "sameAsPrevious"])),
                ],
                &[],
            ),
            json!({"start":1,"source":"userDefined","style":"arabic"}),
        ),
        (
            "variables.define",
            object(
                &[
                    ("name", string()),
                    (
                        "type",
                        choice(&[
                            "custom",
                            "lastPageNumber",
                            "chapterNumber",
                            "fileName",
                            "creationDate",
                            "modificationDate",
                            "outputDate",
                            "runningHeader",
                        ]),
                    ),
                    ("text", string()),
                    ("format", string()),
                    ("style", string()),
                    ("use", choice(&["firstOnPage", "lastOnPage"])),
                    ("character", boolean()),
                    ("section", boolean()),
                    ("extension", boolean()),
                    ("before", string()),
                    ("after", string()),
                ],
                &["name"],
            ),
            json!({"name":"Unit Header","type":"runningHeader","style":"Heading 1"}),
        ),
        ("variables.insert", object(&[("name", string())], &["name"]), json!({"name":"Unit Header"})),
        (
            "layout.marginsAndColumns",
            object(
                &[
                    ("pages", array(index(), 1, 1000)),
                    ("margins", number(0.0, 15552.0)),
                    ("columns", integer(1, 64)),
                    ("gutter", number(0.0, 1000.0)),
                    ("adjustLayout", boolean()),
                    ("parent", json!({"anyOf":[integer(0, 999), string()]})),
                ],
                &[],
            ),
            json!({"margins":54,"columns":1}),
        ),
        (
            "table.insert",
            object(
                &[
                    ("rows", integer(1, 100)),
                    ("cols", integer(1, 32)),
                    ("headerRows", integer(0, 10)),
                    ("footerRows", integer(0, 10)),
                    ("width", dim()),
                ],
                &["rows", "cols"],
            ),
            json!({"rows":4,"cols":3,"width":500}),
        ),
        (
            "table.setCell",
            object(
                &[
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("row", index()),
                    ("col", integer(0, 31)),
                    ("text", string()),
                    ("fill", string()),
                    ("tint", number(0.0, 1.0)),
                    ("insets", edges()),
                    ("verticalJustification", choice(&["top", "center", "bottom", "justify"])),
                ],
                &["row", "col"],
            ),
            json!({"row":0,"col":0,"text":"الاسم"}),
        ),
        (
            "table.insertRow",
            object(
                &[
                    ("where", choice(&["above", "below"])),
                    ("count", integer(1, 100)),
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &[],
            ),
            json!({"story":"$0.story","table":"$1.table","row":1,"where":"below","count":1}),
        ),
        (
            "table.insertColumn",
            object(
                &[
                    ("where", choice(&["left", "right"])),
                    ("count", integer(1, 32)),
                    ("width", dim()),
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &[],
            ),
            json!({"story":"$0.story","table":"$1.table","col":1,"where":"right","count":1}),
        ),
        (
            "table.deleteRow",
            object(
                &[
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &[],
            ),
            json!({"story":"$0.story","table":"$1.table","row":2}),
        ),
        (
            "table.deleteColumn",
            object(
                &[
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &[],
            ),
            json!({"story":"$0.story","table":"$1.table","col":2}),
        ),
        (
            "table.merge",
            object(
                &[
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &[],
            ),
            json!({"story":"$0.story","table":"$1.table","rows":[0,0],"cols":[0,2]}),
        ),
        (
            "table.unmerge",
            object(
                &[
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &[],
            ),
            json!({"story":"$0.story","table":"$1.table","row":0,"col":0}),
        ),
        (
            "table.setRowHeight",
            object(
                &[
                    ("height", dim()),
                    ("mode", choice(&["atLeast", "exactly"])),
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &["height"],
            ),
            json!({"story":"$0.story","table":"$1.table","row":0,"height":28,"mode":"atLeast"}),
        ),
        (
            "table.setColumnWidth",
            object(
                &[
                    ("width", dim()),
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &["width"],
            ),
            json!({"story":"$0.story","table":"$1.table","col":0,"width":160}),
        ),
        (
            "table.distributeColumns",
            object(
                &[
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &[],
            ),
            json!({"story":"$0.story","table":"$1.table","cols":[0,2]}),
        ),
        (
            "table.distributeRows",
            object(&[("story", integer(1, u64::MAX)), ("table", integer(1, u64::MAX)), ("rows", array(index(), 2, 2)), ("row", index())], &[]),
            json!({"story":"$0.story","table":"$1.table","rows":[0,3]}),
        ),
        (
            "table.options",
            object(
                &[
                    ("direction", choice(&["leftToRight", "rightToLeft"])),
                    ("border", cell_stroke_schema()),
                    ("spaceBefore", number(0.0, 1000.0)),
                    ("spaceAfter", number(0.0, 1000.0)),
                    ("headerRows", integer(0, 10)),
                    ("footerRows", integer(0, 10)),
                    ("repeatHeader", boolean()),
                    ("repeatFooter", boolean()),
                    ("altRows", alt_fills_schema()),
                    ("altCols", alt_fills_schema()),
                    ("story", integer(1, u64::MAX)),
                    ("table", integer(1, u64::MAX)),
                    ("rows", array(index(), 2, 2)),
                    ("row", index()),
                    ("cols", array(integer(0, 31), 2, 2)),
                    ("col", integer(0, 31)),
                ],
                &[],
            ),
            json!({"story":"$0.story","table":"$1.table","direction":"rightToLeft","headerRows":1,"repeatHeader":true}),
        ),
        (
            "font.replace",
            object(
                &[
                    ("family", string()),
                    ("style", string()),
                    ("toFamily", string()),
                    ("toStyle", string()),
                    ("redefineStyles", boolean()),
                    ("script", choice(&["arabic", "latin"])),
                    ("pages", array(integer(1, 10000), 2, 2)),
                ],
                &["toFamily"],
            ),
            json!({"script":"arabic","toFamily":"Al-Jazeera-Arabic","pages":[1,32]}),
        ),
        (
            "type.diacritics",
            object(
                &[
                    ("mode", choice(&["none", "strip", "light", "keep"])),
                    ("scope", choice(&["document", "story"])),
                    ("pages", array(integer(1, 10000), 2, 2)),
                    ("story", integer(1, u64::MAX)),
                    ("stories", array(integer(1, u64::MAX), 1, 256)),
                    ("excludeLabels", array(string(), 0, 64)),
                ],
                &["mode"],
            ),
            json!({"mode":"none","pages":[1,32]}),
        ),
        (
            "file.place",
            object(
                &[
                    ("base64", json!({"type":"string","maxLength":8388608})),
                    ("name", json!({"type":"string","maxLength":256})),
                    ("frame", integer(1, u64::MAX)),
                    ("spread", spread_target()),
                    ("x", number(-15552.0, 15552.0)),
                    ("y", number(-15552.0, 15552.0)),
                    ("width", dim()),
                ],
                &["base64", "name"],
            ),
            json!({"base64":"<PNG or JPEG base64>","name":"illustration.png","x":54,"y":200,"width":200}),
        ),
    ]
}

pub(super) fn derived_schema_for_keys(keys: &[&str]) -> Value {
    let mut props = serde_json::Map::new();
    for &key in keys {
        props.entry(key.to_string()).or_insert_with(|| json!({}));
    }
    json!({
        "type": "object",
        "properties": props,
        "required": [],
        "additionalProperties": false,
    })
}

fn is_blocked_for_derived_production(id: &str) -> bool {
    const BLOCKED_PREFIXES: &[&str] = &[
        "file.",
        "document.export",
        "book.",
        "library.",
        "datamerge.",
        "data.",
        "links.",
        "link.",
        "xml.",
        "idml.",
        "epub.",
        "pptx.",
        "script.",
        "production.",
        "prefs.",
        "colorSettings.",
        "preflight.",
        "liquid.",
        "overrides.",
        "media.",
        "place.",
        "conveyor.",
        "view.",
        "color.",
    ];
    const BLOCKED_EXACT: &[&str] = &[
        "style.load",
        "edit.undo",
        "edit.redo",
        "edit.cut",
        "edit.copy",
        "edit.paste",
        "edit.pasteWithoutFormatting",
        "edit.pasteInto",
        "edit.pasteInPlace",
        "edit.pasteSvg",
        "edit.duplicate",
        "object.pasteSvg",
        "story.placeAndLink",
        "story.updateLink",
        "story.unlink",
        "table.dropCells",
        "layout.rotateSpreadView",
        "layout.pages.duplicateSpread",
        "text.release",
        "type.pickUpFormat",
    ];
    BLOCKED_PREFIXES.iter().any(|prefix| id.starts_with(prefix)) || BLOCKED_EXACT.contains(&id)
}

fn safe_derived_keys(spec: &CommandSpec) -> Option<Vec<&'static str>> {
    if !spec.undoable || is_blocked_for_derived_production(spec.id) {
        return None;
    }
    let keys = super::super::params::accepted_keys(spec)?;
    const FORBIDDEN_KEYS: &[&str] = &["path", "paths", "file", "files", "folder", "dir", "directory", "base64", "dataBase64"];
    if keys.iter().any(|k| FORBIDDEN_KEYS.contains(k)) {
        return None;
    }
    Some(keys)
}

/// Complete vetted command schemas (curated + safe `CommandSpec`-derived document commands).
pub(super) fn schemas() -> Vec<(&'static str, Value, Value)> {
    static ALL: OnceLock<Vec<(&'static str, Value, Value)>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut list = curated_schemas();
        let curated_ids: BTreeSet<&'static str> = list.iter().map(|(id, _, _)| *id).collect();
        for spec in super::super::command_specs() {
            if curated_ids.contains(spec.id) {
                continue;
            }
            if let Some(keys) = safe_derived_keys(spec) {
                let schema = super::super::params::command_params_json_schema(spec).unwrap_or_else(|| derived_schema_for_keys(&keys));
                list.push((spec.id, schema, json!({})));
            }
        }
        list
    })
    .clone()
}

/// Parameter JSON Schema for an engine command `id` derived from the unified `CommandSpec` +
/// production schema source of truth (used by MCP and command inspection).
pub fn command_schema(id: &str) -> Option<Value> {
    if let Some((_, schema, _)) = schemas().into_iter().find(|(cmd, _, _)| *cmd == id) {
        return Some(schema);
    }
    let spec = super::super::find_command(id)?;
    super::super::params::command_params_json_schema(spec)
        .or_else(|| super::super::params::accepted_keys(spec).map(|keys| derived_schema_for_keys(&keys)))
}

fn check_known_swatch(doc: &designcraft_doc::Document, id: &str, name: Option<&str>) -> Result<()> {
    if let Some(name) = name
        && !name.is_empty()
        && name != "[None]"
        && !doc.swatches.iter().any(|s| s.name == name)
    {
        return Err(bad(ID, format!("{id}: unknown swatch `{name}`")));
    }
    Ok(())
}

fn check_effects_swatches(doc: &designcraft_doc::Document, id: &str, effects: &Value) -> Result<()> {
    for key in ["dropShadow", "innerShadow", "outerGlow", "innerGlow", "satin"] {
        if let Some(effect) = effects.get(key) {
            check_known_swatch(doc, id, effect.get("color").and_then(Value::as_str))?;
        }
    }
    if let Some(bevel) = effects.get("bevel") {
        check_known_swatch(doc, id, bevel.get("highlight").and_then(Value::as_str))?;
        check_known_swatch(doc, id, bevel.get("shadow").and_then(Value::as_str))?;
    }
    Ok(())
}

pub(super) fn check_command_swatches(doc: &designcraft_doc::Document, id: &str, resolved: &Value) -> Result<()> {
    match id {
        "object.dropShadow" | "object.qrCode" => {
            check_known_swatch(doc, id, resolved.get("color").and_then(Value::as_str))?;
        }
        "object.gradient" => {
            if let Some(stops) = resolved.get("stops").and_then(Value::as_array) {
                for stop in stops {
                    check_known_swatch(doc, id, stop.get("swatch").and_then(Value::as_str))?;
                }
            }
        }
        "table.setCell" => {
            check_known_swatch(doc, id, resolved.get("fill").and_then(Value::as_str))?;
        }
        "find.change" => {
            if let Some(attrs) = resolved.get("attrs") {
                check_known_swatch(doc, id, attrs.get("fill").and_then(Value::as_str))?;
            }
        }
        "find.changeObjects" => {
            if let Some(ch) = resolved.get("change") {
                check_known_swatch(doc, id, ch.get("fill").and_then(Value::as_str))?;
                check_known_swatch(doc, id, ch.get("stroke").and_then(Value::as_str))?;
            }
        }
        "style.object.create" => {
            check_known_swatch(doc, id, resolved.get("fill").and_then(Value::as_str))?;
            if let Some(shadow) = resolved.get("dropShadow") {
                check_known_swatch(doc, id, shadow.get("color").and_then(Value::as_str))?;
            }
            if let Some(effects) = resolved.get("effects") {
                check_effects_swatches(doc, id, effects)?;
            }
        }
        "object.effects" => {
            if let Some(effects) = resolved.get("effects") {
                check_effects_swatches(doc, id, effects)?;
            }
        }
        "style.cell.create" => {
            check_known_swatch(doc, id, resolved.get("fill").and_then(Value::as_str))?;
            if let Some(stroke) = resolved.get("stroke") {
                check_known_swatch(doc, id, stroke.get("color").and_then(Value::as_str))?;
            }
        }
        "style.table.create" | "table.options" => {
            if let Some(border) = resolved.get("border") {
                check_known_swatch(doc, id, border.get("color").and_then(Value::as_str))?;
            }
            for key in ["altRows", "altCols"] {
                if let Some(alt) = resolved.get(key) {
                    check_known_swatch(doc, id, alt.get("firstColor").and_then(Value::as_str))?;
                    check_known_swatch(doc, id, alt.get("nextColor").and_then(Value::as_str))?;
                }
            }
        }
        "type.para" => {
            if let Some(attrs) = resolved.get("attrs") {
                check_known_swatch(doc, id, attrs.get("shadingColor").and_then(Value::as_str))?;
                check_known_swatch(doc, id, attrs.get("borderColor").and_then(Value::as_str))?;
            }
        }
        "style.paragraph.create" | "style.paragraph.edit" | "style.character.create" | "style.character.edit" => {
            if let Some(chars) = resolved.get("chars") {
                check_known_swatch(doc, id, chars.get("fill").and_then(Value::as_str))?;
            }
            if let Some(para) = resolved.get("para") {
                check_known_swatch(doc, id, para.get("shadingColor").and_then(Value::as_str))?;
                check_known_swatch(doc, id, para.get("borderColor").and_then(Value::as_str))?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_bounded_value(value: &Value, path: &str, depth: usize) -> Result<()> {
    if depth > 6 {
        return Err(bad(ID, format!("{path}: parameter nesting too deep")));
    }
    match value {
        Value::Null | Value::Bool(_) => Ok(()),
        Value::Number(n) => {
            if n.as_u64().is_some() || n.as_i64().is_some() || n.as_f64().is_some_and(|v| v.is_finite() && (-1_000_000.0..=1_000_000.0).contains(&v))
            {
                Ok(())
            } else {
                Err(bad(ID, format!("{path}: outside permitted range")))
            }
        }
        Value::String(text) => {
            if text.chars().count() > 262_144 {
                Err(bad(ID, format!("{path}: text too long")))
            } else {
                Ok(())
            }
        }
        Value::Array(items) => {
            if items.len() > 1024 {
                return Err(bad(ID, format!("{path}: invalid array length")));
            }
            for (idx, item) in items.iter().enumerate() {
                check_bounded_value(item, &format!("{path}[{idx}]"), depth + 1)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            if map.len() > 256 {
                return Err(bad(ID, format!("{path}: too many object fields")));
            }
            for (key, val) in map {
                check_bounded_value(val, &format!("{path}.{key}"), depth + 1)?;
            }
            Ok(())
        }
    }
}

pub(super) fn check(schema: &Value, value: &Value, path: &str) -> Result<()> {
    if schema.get("type").is_none() && schema.get("anyOf").is_none() {
        return check_bounded_value(value, path, 0);
    }
    if let Some(options) = schema.get("anyOf").and_then(Value::as_array) {
        return if options.iter().any(|s| check(s, value, path).is_ok()) {
            Ok(())
        } else {
            Err(bad(ID, format!("{path}: no permitted type matches")))
        };
    }
    let valid = match schema.get("type").and_then(Value::as_str) {
        Some("object") => value.is_object(),
        Some("array") => value.is_array(),
        Some("string") => value.is_string(),
        Some("boolean") => value.is_boolean(),
        Some("integer") => value.as_u64().is_some(),
        Some("number") => value.as_f64().is_some_and(f64::is_finite),
        Some("null") => value.is_null(),
        _ => false,
    };
    if !valid {
        return Err(bad(ID, format!("{path}: invalid parameter type")));
    }
    if let Some(options) = schema.get("enum").and_then(Value::as_array)
        && !options.contains(value)
    {
        return Err(bad(ID, format!("{path}: unsupported value")));
    }
    if let Some(n) = value.as_f64()
        && (schema.get("minimum").and_then(Value::as_f64).is_some_and(|min| n < min)
            || schema.get("maximum").and_then(Value::as_f64).is_some_and(|max| n > max))
    {
        return Err(bad(ID, format!("{path}: outside permitted range")));
    }
    if let Some(text) = value.as_str() {
        if schema.get("minLength").and_then(Value::as_u64).is_some_and(|min| (text.chars().count() as u64) < min) {
            return Err(bad(ID, format!("{path}: text too short")));
        }
        if schema.get("maxLength").and_then(Value::as_u64).is_some_and(|max| text.chars().count() as u64 > max) {
            return Err(bad(ID, format!("{path}: text too long")));
        }
    }
    if let Some(values) = value.as_array() {
        if schema.get("minItems").and_then(Value::as_u64).is_some_and(|min| (values.len() as u64) < min)
            || schema.get("maxItems").and_then(Value::as_u64).is_some_and(|max| values.len() as u64 > max)
        {
            return Err(bad(ID, format!("{path}: invalid array length")));
        }
        let item = schema.get("items").ok_or_else(|| bad(ID, "missing item schema"))?;
        for (idx, v) in values.iter().enumerate() {
            check(item, v, &format!("{path}[{idx}]"))?;
        }
    }
    if let Some(values) = value.as_object() {
        if let Some(props) = schema.get("properties").and_then(Value::as_object) {
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for key in required.iter().filter_map(Value::as_str) {
                    if !values.contains_key(key) {
                        return Err(bad(ID, format!("{path}: missing `{key}`")));
                    }
                }
            }
            for (key, v) in values {
                check(props.get(key).ok_or_else(|| bad(ID, format!("{path}: unsupported parameter `{key}`")))?, v, &format!("{path}.{key}"))?;
            }
        } else if schema.get("additionalProperties") == Some(&Value::Bool(true)) {
            if let Some(val_schema) = schema.get("propertyValues") {
                for (key, v) in values {
                    check(val_schema, v, &format!("{path}.{key}"))?;
                }
            }
        } else {
            return Err(bad(ID, "missing object schema"));
        }
    }
    Ok(())
}
