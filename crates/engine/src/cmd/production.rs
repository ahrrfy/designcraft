//! Restricted, native AI document production. Plans execute on a private draft; only a complete
//! successful draft is published. Neither provider output nor a caller's allowlist grants IO.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

use super::{CommandSpec, always, bad, cmd, has_doc};
use crate::{HistoryEntry, Result, Session};

const ID: &str = "production.apply";
const MAX_ACTIONS: usize = 512;
const MAX_BYTES: usize = 48 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_ENCODED_IMAGE_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
const MAX_IMAGE_PIXELS: u64 = 20_000_000;
const MAX_UNIQUE_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_UNIQUE_IMAGE_PIXELS: u64 = 40_000_000;
const GROUP_ID: &str = "production.applyGroup";
const MAX_GROUPS: usize = 1000;
const MAX_GROUP_BYTES: usize = 512 * 1024 * 1024;

fn object(fields: &[(&str, Value)], required: &[&str]) -> Value {
    json!({"type":"object", "properties": fields.iter().map(|(k,v)| (k.to_string(),v.clone())).collect::<serde_json::Map<_,_>>(),
        "required":required,"additionalProperties":false})
}
fn number(min: f64, max: f64) -> Value {
    json!({"type":"number","minimum":min,"maximum":max})
}
fn integer(min: u64, max: u64) -> Value {
    json!({"type":"integer","minimum":min,"maximum":max})
}
fn string() -> Value {
    json!({"type":"string","maxLength":262144})
}
fn boolean() -> Value {
    json!({"type":"boolean"})
}
fn choice(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}
fn array(items: Value, min: usize, max: usize) -> Value {
    json!({"type":"array","items":items,"minItems":min,"maxItems":max})
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

/// Vetted parameter schemas. Deliberately narrower than the general command interface.
fn schemas() -> Vec<(&'static str, Value, Value)> {
    let dim = || number(0.01, 15552.0);
    let index = || integer(0, 999);
    let edges = || json!({"anyOf":[number(0.0,15552.0),array(number(0.0,15552.0),4,4)]});
    let blend_mode = || choice(&["normal", "multiply", "screen", "overlay", "darken", "lighten"]);
    vec![
        (
            "frame.create",
            object(
                &[
                    ("rect", rect()),
                    ("spread", index()),
                    ("shape", choice(&["rectangle", "ellipse", "polygon"])),
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
        ("line.create", object(&[("a", point()), ("b", point()), ("spread", index())], &["a", "b"]), json!({"a":[54,160],"b":[540,160]})),
        (
            "path.create",
            object(
                &[
                    ("spread", index()),
                    ("closed", boolean()),
                    ("anchors", array(object(&[("p", point()), ("in", point()), ("out", point())], &["p"]), 2, 1024)),
                ],
                &["anchors"],
            ),
            json!({"anchors":[{"p":[54,54]},{"p":[88,54]},{"p":[71,78]}],"closed":true}),
        ),
        ("object.label", object(&[("ids", ids()), ("label", string())], &["ids", "label"]), json!({"ids":["$0.id"],"label":"lesson.heading"})),
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
                    ("spread", index()),
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
                &[("from", integer(1, u64::MAX)), ("to", integer(1, u64::MAX)), ("rect", rect()), ("spread", index()), ("autoflow", boolean())],
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
                    ("caseSensitive", boolean()),
                    ("wholeWord", boolean()),
                    ("scope", choice(&["document", "story"])),
                    ("story", integer(1, u64::MAX)),
                    ("first", boolean()),
                ],
                &["find", "change"],
            ),
            json!({"find":"النص القديم","change":"النص الجديد","scope":"document"}),
        ),
        ("type.char", object(&[("attrs", char_attrs_schema())], &["attrs"]), json!({"attrs":{"size":14,"fontFamily":"Arial"}})),
        ("type.para", object(&[("attrs", para_attrs_schema())], &["attrs"]), json!({"attrs":{"align":"right","direction":"rightToLeft"}})),
        (
            "list.define",
            object(&[("name", string()), ("continueAcrossStories", boolean()), ("rename", string())], &["name"]),
            json!({"name":"Lesson Steps","continueAcrossStories":true}),
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
                    ("altCols", alt_fills_schema()),
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
        ("layout.pages.insert", object(&[("count", integer(1, 100)), ("after", index())], &["count"]), json!({"count":1})),
        (
            "layout.marginsAndColumns",
            object(
                &[
                    ("pages", array(index(), 1, 1000)),
                    ("margins", number(0.0, 15552.0)),
                    ("columns", integer(1, 64)),
                    ("gutter", number(0.0, 1000.0)),
                    ("adjustLayout", boolean()),
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
            "file.place",
            object(
                &[
                    ("base64", json!({"type":"string","maxLength":8388608})),
                    ("name", json!({"type":"string","maxLength":256})),
                    ("frame", integer(1, u64::MAX)),
                    ("spread", index()),
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

fn check_command_swatches(doc: &designcraft_doc::Document, id: &str, resolved: &Value) -> Result<()> {
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

fn item_contains_graphic(item: &designcraft_doc::Item) -> bool {
    let mut stack = vec![item];
    while let Some(current) = stack.pop() {
        if current.graphic().is_some() {
            return true;
        }
        stack.extend(current.children().iter().map(AsRef::as_ref));
    }
    false
}

/// Catalog consumed by prompt builders and the AI permissions UI.
pub fn catalog() -> Value {
    let commands: Vec<_> = schemas()
        .into_iter()
        .map(|(id, schema, example)| {
            json!({"command":id,"paramsSchema":schema,"example":example,
        "description":super::find_command(id).map(|s| s.params),"requiresImages":id=="file.place"})
        })
        .collect();
    json!({"version":1,"outputModes":["native_only","native_with_images"],"maxActions":MAX_ACTIONS,
        "references":"$N.path and ${N.path}; zero-based earlier results, resolved values are revalidated",
        "commands":commands})
}

fn check(schema: &Value, value: &Value, path: &str) -> Result<()> {
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
        for v in values {
            check(item, v, path)?;
        }
    }
    if let Some(values) = value.as_object() {
        let props = schema.get("properties").and_then(Value::as_object).ok_or_else(|| bad(ID, "missing object schema"))?;
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
    }
    Ok(())
}

// Iterative walk bounds recursion before calling the script reference resolver or cloning JSON.
fn bounded(value: &Value) -> Result<()> {
    bounded_limits(value, ID, MAX_BYTES, 65536, 24)
}

fn bounded_limits(value: &Value, id: &str, max_bytes: usize, max_nodes: usize, max_depth: usize) -> Result<()> {
    let mut stack = vec![(value, 0usize)];
    let mut bytes = 0usize;
    let mut nodes = 0usize;
    while let Some((v, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if depth > max_depth || nodes > max_nodes {
            return Err(bad(id, "plan nesting or node limit exceeded"));
        }
        match v {
            Value::String(s) => bytes = bytes.saturating_add(s.len()),
            Value::Array(a) => {
                if a.len() > 65536 {
                    return Err(bad(id, "array too large"));
                }
                stack.extend(a.iter().map(|x| (x, depth + 1)));
            }
            Value::Object(o) => {
                if o.len() > 128 {
                    return Err(bad(id, "object too large"));
                }
                for (key, x) in o {
                    bytes = bytes.saturating_add(key.len());
                    stack.push((x, depth + 1));
                }
            }
            _ => {}
        }
        if bytes > max_bytes {
            return Err(bad(id, "plan byte limit exceeded"));
        }
    }
    Ok(())
}

fn draft(s: &Session, p: &Value, mut cancelled: impl FnMut() -> bool) -> Result<(crate::DocState, Vec<Value>)> {
    bounded(p)?;
    let params = p.as_object().ok_or_else(|| bad(ID, "parameters must be an object"))?;
    for key in params.keys() {
        if !["commands", "outputMode", "allowedCommands", "label", "expectedDocumentUid", "expectedRevision", "requirePreflight", "minPpi"]
            .contains(&key.as_str())
        {
            return Err(bad(ID, format!("unknown parameter `{key}`")));
        }
    }
    let require_preflight = match p.get("requirePreflight") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| bad(ID, "requirePreflight must be a boolean"))?,
    };
    let min_ppi = match p.get("minPpi") {
        None => 150.0,
        Some(v) => v
            .as_f64()
            .filter(|n| n.is_finite() && (1.0..=2400.0).contains(n))
            .ok_or_else(|| bad(ID, "minPpi must be a finite number from 1 to 2400"))?,
    };
    let mode = p.get("outputMode").and_then(Value::as_str).ok_or_else(|| bad(ID, "outputMode required"))?;
    let images = match mode {
        "native_only" | "nativeOnly" => false,
        "native_with_images" | "nativeWithImages" => true,
        _ => return Err(bad(ID, "invalid outputMode")),
    };
    let actions = p
        .get("commands")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty() && a.len() <= MAX_ACTIONS)
        .ok_or_else(|| bad(ID, "commands must contain 1 to 512 actions"))?;
    let allowed = match p.get("allowedCommands") {
        None => None,
        Some(Value::Array(a)) if a.iter().all(Value::is_string) && a.len() <= 128 => Some(a),
        _ => return Err(bad(ID, "allowedCommands must be a string array")),
    };
    if let Some(label) = p.get("label")
        && !label.as_str().is_some_and(|s| !s.trim().is_empty() && s.len() <= 256)
    {
        return Err(bad(ID, "label must contain 1 to 256 bytes"));
    }
    let original = s.doc()?;
    if original.interaction.is_some() || original.preview_stash.is_some() || s.tool.composing() {
        return Err(bad(ID, "finish the current interaction or preview first"));
    }
    for (key, actual) in [("expectedDocumentUid", original.uid), ("expectedRevision", original.revision)] {
        if let Some(v) = p.get(key)
            && v.as_u64() != Some(actual)
        {
            return Err(bad(ID, format!("{key}: document changed while generation was pending")));
        }
    }
    let mut vetted = schemas();
    // Providers cannot author file.place. Keep their public catalog stable,
    // while validated host-generated inline assets receive the print allowance.
    let image_schema = vetted.iter_mut().find(|(id, _, _)| *id == "file.place").ok_or_else(|| bad(ID, "missing inline image schema"))?;
    let encoded_limit = image_schema
        .1
        .get_mut("properties")
        .and_then(|properties| properties.get_mut("base64"))
        .and_then(|base64| base64.get_mut("maxLength"))
        .ok_or_else(|| bad(ID, "missing inline image byte limit"))?;
    *encoded_limit = json!(MAX_ENCODED_IMAGE_BYTES);
    // Reject every command and malformed envelope before executing the first draft action.
    for action in actions {
        let a = action.as_object().ok_or_else(|| bad(ID, "each action must be an object"))?;
        if a.keys().any(|k| k != "command" && k != "params") {
            return Err(bad(ID, "unsupported action property"));
        }
        let id = action.get("command").and_then(Value::as_str).ok_or_else(|| bad(ID, "action command required"))?;
        if !vetted.iter().any(|(c, _, _)| *c == id)
            || (!images && id == "file.place")
            || allowed.is_some_and(|a| !a.iter().any(|c| c.as_str() == Some(id)))
        {
            return Err(bad(ID, format!("command `{id}` is not permitted")));
        }
        if !action.get("params").is_some_and(Value::is_object) {
            return Err(bad(ID, "action params must be an object"));
        }
    }
    let mut work = Session::new();
    work.cache = s.cache.clone();
    work.prefs = s.prefs.clone();
    work.docs.push(original.clone());
    work.active = Some(0);
    // A draft needs no per-action history or journal retention.
    work.in_undo_step = true;
    let mut results = Vec::with_capacity(actions.len());
    let mut image_sources = BTreeSet::new();
    let mut placed_raster_frames = BTreeSet::new();
    let mut image_bytes = 0usize;
    let mut image_pixels = 0u64;
    let mut table_cells = 0u64;
    for (i, action) in actions.iter().enumerate() {
        if cancelled() {
            return Err(bad(ID, "production cancelled"));
        }
        let id = action.get("command").and_then(Value::as_str).ok_or_else(|| bad(ID, "missing command"))?;
        let raw = action.get("params").ok_or_else(|| bad(ID, "missing params"))?;
        let mut resolved = crate::script::resolve(raw, &results).map_err(|e| bad(ID, format!("step {i}: {e}")))?;
        bounded(&resolved)?;
        let schema = vetted.iter().find(|(c, _, _)| *c == id).ok_or_else(|| bad(ID, "command not vetted"))?;
        check(&schema.1, &resolved, id)?;
        check_command_swatches(&work.doc()?.doc, id, &resolved)?;
        if id == "object.gradient"
            && let Some(stops) = resolved.get_mut("stops").and_then(Value::as_array_mut)
        {
            for stop in stops {
                if let Some(st) = stop.as_object_mut() {
                    if !st.contains_key("location")
                        && let Some(pos) = st.get("pos").and_then(Value::as_f64)
                    {
                        st.insert("location".into(), json!(pos * 100.0));
                    }
                    if st.get("color").is_none() && st.get("swatch").is_none() {
                        return Err(bad(ID, "object.gradient.stops: missing `color` or `swatch`"));
                    }
                }
            }
        }
        if id == "object.gradientFeather"
            && let Some(obj) = resolved.as_object_mut()
        {
            let s_val = obj.get("start").and_then(Value::as_f64);
            let e_val = obj.get("end").and_then(Value::as_f64);
            let unit_opacities = (s_val.is_some() || e_val.is_some())
                && s_val.is_none_or(|v| (0.0..=1.0).contains(&v))
                && e_val.is_none_or(|v| (0.0..=1.0).contains(&v))
                && (s_val.unwrap_or(0.0) > 0.0 || e_val.unwrap_or(0.0) > 0.0);
            if unit_opacities {
                if let Some(sv) = s_val {
                    obj.insert("start".into(), json!(sv * 100.0));
                }
                if let Some(ev) = e_val {
                    obj.insert("end".into(), json!(ev * 100.0));
                }
            }
        }
        if id == "transform.move" && resolved.get("copy").and_then(Value::as_bool) == Some(true) {
            let targets = resolved.get("ids").and_then(Value::as_array).ok_or_else(|| bad(ID, "raster copy needs explicit targets"))?;
            let source = targets.first().and_then(Value::as_u64).filter(|_| targets.len() == 1);
            let current = work.doc()?;
            if !images
                || !source.is_some_and(|source| {
                    placed_raster_frames.contains(&source)
                        && current
                            .doc
                            .item(designcraft_doc::ItemId(source))
                            .is_some_and(|item| item.graphic().is_some() && item.children().is_empty())
                })
            {
                return Err(bad(ID, "production copy requires one validated raster leaf placed in this private draft"));
            }
        }
        if id == "edit.stepAndRepeat" {
            let current = work.doc()?;
            let target_ids: Vec<designcraft_doc::ItemId> = if let Some(arr) = resolved.get("ids").and_then(Value::as_array) {
                arr.iter().filter_map(Value::as_u64).map(designcraft_doc::ItemId).collect()
            } else {
                current.selection.items.clone()
            };
            if target_ids.iter().any(|&item_id| current.doc.item(item_id).is_some_and(item_contains_graphic)) {
                return Err(bad(ID, "edit.stepAndRepeat cannot duplicate graphic frames in production"));
            }
        }
        if matches!(id, "frame.create" | "object.qrCode" | "text.thread")
            && let Some(rect_arr) = resolved.get("rect").and_then(Value::as_array)
        {
            let coordinates: Vec<_> = rect_arr.iter().filter_map(Value::as_f64).collect();
            if coordinates.get(2).zip(coordinates.first()).is_none_or(|(right, left)| right <= left)
                || coordinates.get(3).zip(coordinates.get(1)).is_none_or(|(bottom, top)| bottom <= top)
            {
                return Err(bad(ID, "frame rectangle must have positive width and height"));
            }
        }
        if id == "file.place" {
            let raster = check_image(&resolved)?;
            if image_sources.insert(raster.sha256) {
                image_bytes = image_bytes
                    .checked_add(raster.bytes)
                    .filter(|bytes| *bytes <= MAX_UNIQUE_IMAGE_BYTES)
                    .ok_or_else(|| bad(ID, "plan exceeds 32 MiB of unique raster bytes"))?;
                image_pixels = image_pixels
                    .checked_add(raster.pixels)
                    .filter(|pixels| *pixels <= MAX_UNIQUE_IMAGE_PIXELS)
                    .ok_or_else(|| bad(ID, "plan exceeds 40 million unique image pixels"))?;
            }
        }
        if id == "table.insert" {
            let n = |key| resolved.get(key).and_then(Value::as_u64).unwrap_or(0);
            table_cells =
                table_cells.saturating_add(n("rows").saturating_add(n("headerRows")).saturating_add(n("footerRows")).saturating_mul(n("cols")));
            if table_cells > 10000 {
                return Err(bad(ID, "plan exceeds 10000 table cells"));
            }
        }
        if matches!(id, "table.insertRow" | "table.insertColumn") {
            let count = resolved.get("count").and_then(Value::as_u64).unwrap_or(1);
            table_cells = table_cells.saturating_add(count.saturating_mul(32));
            if table_cells > 10000 {
                return Err(bad(ID, "plan exceeds 10000 table cells"));
            }
        }
        if matches!(
            id,
            "table.setCell"
                | "table.insertRow"
                | "table.insertColumn"
                | "table.deleteRow"
                | "table.deleteColumn"
                | "table.merge"
                | "table.unmerge"
                | "table.setRowHeight"
                | "table.setColumnWidth"
                | "table.distributeColumns"
                | "table.distributeRows"
                | "table.options"
                | "style.table.apply"
                | "style.cell.apply"
        ) && work.doc()?.selection.text.and_then(|t| t.cell).is_none()
            && let Some(table) = resolved.get("table").and_then(Value::as_u64)
        {
            let mut select_params = json!({"table": table});
            for key in ["story", "row", "rows", "col", "cols"] {
                if let Some(v) = resolved.get(key) {
                    select_params[key] = v.clone();
                }
            }
            work.execute("table.select", &select_params).map_err(|e| bad(ID, format!("step {i} ({id}): {e}")))?;
        }
        let result = work.execute(id, &resolved).map_err(|e| bad(ID, format!("step {i} ({id}): {e}")))?;
        if id == "file.place" {
            let frame = result.get("id").and_then(Value::as_u64).ok_or_else(|| bad(ID, "validated raster placement did not return a frame"))?;
            placed_raster_frames.insert(frame);
        }
        if work.doc()?.doc.page_count() > 1000 {
            return Err(bad(ID, "production page limit exceeded"));
        }
        work.journal.clear();
        results.push(result);
    }
    if require_preflight {
        check_text_layout(s, &work)?;
        // Gate the private draft before publication. Missing fonts/glyphs, overset text and
        // other error-level defects cannot leave a partially accepted live document behind.
        let issues = super::preflight::check(&work, min_ppi);
        let errors: Vec<_> = issues.iter().filter(|issue| issue.severity == "error").collect();
        if !errors.is_empty() {
            let summary = errors.iter().take(8).map(|issue| format!("{}: {}", issue.kind, issue.message)).collect::<Vec<_>>().join("; ");
            return Err(bad(ID, format!("preflight rejected draft ({} errors): {summary}", errors.len())));
        }
    }
    if cancelled() {
        return Err(bad(ID, "production cancelled"));
    }
    Ok((work.doc()?.clone(), results))
}

/// A rendered document-page instance. A parent frame can have an instance on several pages.
struct LayoutFrame<'a> {
    item: &'a designcraft_doc::Item,
    loc: designcraft_doc::ItemLoc,
    story: &'a std::sync::Arc<designcraft_doc::Story>,
    parent: designcraft_geom::Affine,
    bounds: designcraft_geom::Rect,
    safe: designcraft_geom::Rect,
    spread_index: usize,
    paint_order: usize,
    page: usize,
    page_id: u64,
}

/// An opaque non-text object rendered on a document spread in paint order.
struct OpaqueOccluder<'a> {
    item: &'a designcraft_doc::Item,
    loc: designcraft_doc::ItemLoc,
    parent: designcraft_geom::Affine,
    bounds: designcraft_geom::Rect,
    spread_index: usize,
    forced_page: Option<usize>,
    paint_order: usize,
}

fn layout_scene(s: &Session) -> Result<(Vec<LayoutFrame<'_>>, Vec<OpaqueOccluder<'_>>)> {
    use designcraft_doc::{Content, ItemLoc, Shape, SpreadRef};
    use designcraft_geom::Affine;
    let d = &s.doc()?.doc;
    // Validate the complete tree before any location lookup; imported hidden groups count too.
    if d.spreads.len() > 1000 || d.parents.len() > 1024 {
        return Err(bad(ID, "textLayoutLimit: too many document or parent spreads"));
    }
    let mut tree: Vec<_> =
        d.spread_refs().filter_map(|r| d.spread(r)).flat_map(|sp| sp.items.iter().map(|it| (it.as_ref(), 0usize))).take(65537).collect();
    if tree.len() > 65536 {
        return Err(bad(ID, "textLayoutLimit: more than 65536 document items"));
    }
    let mut count = 0usize;
    while let Some((item, depth)) = tree.pop() {
        count = count.saturating_add(1);
        if count > 65536 || depth > 64 {
            return Err(bad(ID, "textLayoutLimit: document exceeds 65536 items or 64 group levels"));
        }
        let remaining = 65537usize.saturating_sub(tree.len());
        tree.extend(item.children().iter().take(remaining).map(|child| (child.as_ref(), depth + 1)));
        if tree.len() > 65536 {
            return Err(bad(ID, "textLayoutLimit: more than 65536 pending document items"));
        }
    }
    // Walk each document spread in exact Renderer::draw_spread back-to-front paint order:
    // layers back-to-front (`d.layers.iter().rev()`), inherited parent items first, then spread items,
    // and group children in forward `0..children.len()` order (via `.rev()` on a LIFO stack).
    let mut frames = Vec::new();
    let mut occluders = Vec::new();
    let mut visited = 0usize;
    let known_layers: std::collections::HashSet<_> = d.layers.iter().map(|l| l.id).collect();

    for (si, sp) in d.spreads.iter().enumerate() {
        let sr = SpreadRef::Doc(si);
        let mut spread_roots = Vec::new();
        for layer in d.layers.iter().rev() {
            let layer_vis = layer.visible && layer.printable;
            for (pi, page) in sp.pages.iter().enumerate().filter(|(_, page)| page.show_parent_items) {
                for (ppi, ppage) in d.shown_parent_pages(sr, pi) {
                    let Some(parent) = d.parents.get(ppi) else { continue };
                    let Some(ppg) = parent.pages.get(ppage) else { continue };
                    for (index, item) in parent.items.iter().enumerate() {
                        if item.layer != layer.id
                            || page.overridden.contains(&item.id)
                            || (parent.pages.len() > 1 && parent.page_at_x(item.bounds().center().x) != Some(ppage))
                        {
                            continue;
                        }
                        if spread_roots.len() >= 65536 {
                            return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
                        }
                        spread_roots.push((
                            ItemLoc { spread: SpreadRef::Parent(ppi), path: vec![index] },
                            Affine::translate((page.x - ppg.x, 0.0)),
                            Some(d.first_page_of_spread(si) + pi),
                            layer_vis,
                            true,
                            1.0_f32,
                            true,
                        ));
                    }
                }
            }
            for (index, item) in sp.items.iter().enumerate() {
                if item.layer != layer.id {
                    continue;
                }
                if spread_roots.len() >= 65536 {
                    return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
                }
                spread_roots.push((ItemLoc { spread: sr, path: vec![index] }, Affine::IDENTITY, None, layer_vis, true, 1.0_f32, true));
            }
        }
        // Include any items whose layer id is missing from `d.layers` (as non-visible) so instance bounds remain consistent.
        for (index, item) in sp.items.iter().enumerate() {
            if !known_layers.contains(&item.layer) {
                if spread_roots.len() >= 65536 {
                    return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
                }
                spread_roots.push((ItemLoc { spread: sr, path: vec![index] }, Affine::IDENTITY, None, false, true, 1.0_f32, true));
            }
        }

        let mut pending: Vec<_> = spread_roots.into_iter().rev().collect();
        let mut next_paint_order = 0usize;
        while let Some((loc, placement, forced_page, inherited_vis, active_state, inherited_opacity, inherited_normal_blend)) = pending.pop() {
            visited = visited.saturating_add(1);
            if visited > 65536 || pending.len() > 65536 {
                return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
            }
            let item = d.item_at(&loc).ok_or_else(|| bad(ID, "textLayoutLocation: invalid item location"))?;
            let item_layer_vis = d.layer(item.layer).is_some_and(|layer| layer.visible && layer.printable);
            let visible = inherited_vis && item_layer_vis && !item.hidden && !item.nonprinting && item.opacity.is_finite() && item.opacity > 0.0;
            let item_opacity = if item.opacity.is_finite() { item.opacity.clamp(0.0, 1.0) } else { 0.0 };
            let eff_opacity = inherited_opacity * item_opacity;
            let eff_normal_blend = inherited_normal_blend && item.blend == designcraft_color::BlendMode::Normal;
            let paint_order = next_paint_order;
            next_paint_order = next_paint_order.saturating_add(1);

            let only_state = (!item.states.is_empty()).then_some(item.active_state);
            for (index, _) in item.children().iter().enumerate().rev() {
                if pending.len() >= 65536 {
                    return Err(bad(ID, "textLayoutLimit: too many rendered item instances"));
                }
                let mut child = loc.clone();
                child.path.push(index);
                let child_active = active_state && only_state.is_none_or(|st| st == index);
                pending.push((child, placement, forced_page, visible, child_active, eff_opacity, eff_normal_blend));
            }
            if !visible {
                continue;
            }
            let parent = placement * d.parent_xf(&loc);
            if let Some(tf) = item.text_frame() {
                let story = d.stories.get(&tf.story).ok_or_else(|| bad(ID, format!("textLayoutStory: item {} has no story", item.id.0)))?;
                let composed = s.cache.get(d, tf.story, None);
                let nonempty =
                    composed.frame(item.id).is_some_and(|frame| story.text.get(frame.range.clone()).is_some_and(|text| !text.trim().is_empty()));
                if !nonempty {
                    continue;
                }
                let bounds = parent.transform_rect_bbox(item.bounds());
                if ![bounds.x0, bounds.y0, bounds.x1, bounds.y1].iter().all(|v| v.is_finite()) {
                    return Err(bad(ID, format!("textLayoutGeometry: item {} has nonfinite bounds", item.id.0)));
                }
                let page = if let Some(page) = forced_page {
                    page
                } else {
                    let SpreadRef::Doc(doc_si) = loc.spread else {
                        return Err(bad(ID, "textLayoutLocation: parent item without an owning page"));
                    };
                    d.first_page_of_spread(doc_si)
                        + d.spread(loc.spread)
                            .and_then(|s_sp| s_sp.page_at_x(bounds.center().x))
                            .ok_or_else(|| bad(ID, format!("textOutsideMargins: item {} has no owning page", item.id.0)))?
                };
                let owning = d.page(page).ok_or_else(|| bad(ID, "textLayoutLocation: no owning page"))?;
                frames.push(LayoutFrame {
                    item,
                    loc,
                    story,
                    parent,
                    bounds,
                    safe: owning.margin_rect(),
                    spread_index: si,
                    paint_order,
                    page,
                    page_id: owning.id.0,
                });
                if frames.len() > 4096 {
                    return Err(bad(ID, "textLayoutLimit: more than 4096 nonempty printable text-frame instances"));
                }
            } else if active_state
                && !item.is_group()
                && item.shape != Shape::GraphicLine
                && (item.shape != Shape::Path || item.path.subpaths.iter().any(|sub| sub.closed))
                && eff_opacity >= 0.9
                && eff_normal_blend
            {
                let has_opaque_fill = !item.fill.is_none() && item.fill.tint.is_finite() && item.fill.tint >= 0.9;
                let occ_bounds = if has_opaque_fill {
                    Some(parent.transform_rect_bbox(item.bounds()))
                } else if let Content::Graphic(g) = &item.content {
                    g.shown_bounds(item.inner_bounds()).map(|r| (parent * item.xf).transform_rect_bbox(r))
                } else {
                    None
                };
                if let Some(bounds) = occ_bounds.filter(|b| [b.x0, b.y0, b.x1, b.y1].iter().all(|v| v.is_finite()))
                    && bounds.width() > 0.0
                    && bounds.height() > 0.0
                {
                    occluders.push(OpaqueOccluder { item, loc, parent, bounds, spread_index: si, forced_page, paint_order });
                }
            }
        }
    }
    Ok((frames, occluders))
}

fn check_text_layout(original: &Session, draft: &Session) -> Result<()> {
    const TOLERANCE: f64 = 0.25;
    const MIN_OCCLUSION_DIM_PT: f64 = 4.0;
    const MIN_OCCLUSION_RATIO: f64 = 0.35;
    let (before, before_occluders) = layout_scene(original)?;
    let (after, after_occluders) = layout_scene(draft)?;
    let previous: std::collections::HashMap<_, _> = before.iter().map(|frame| ((frame.item.id, frame.page_id), frame)).collect();
    let previous_occluders: std::collections::HashMap<_, _> =
        before_occluders.iter().map(|occ| ((occ.item.id, occ.spread_index, occ.forced_page), occ)).collect();
    let changed: Vec<_> = after
        .iter()
        .map(|frame| {
            previous.get(&(frame.item.id, frame.page_id)).is_none_or(|old| {
                old.item != frame.item
                    || !std::sync::Arc::ptr_eq(old.story, frame.story)
                    || old.parent != frame.parent
                    || old.safe != frame.safe
                    || old.bounds != frame.bounds
            })
        })
        .collect();
    let occluder_changed: Vec<_> = after_occluders
        .iter()
        .map(|occ| {
            previous_occluders
                .get(&(occ.item.id, occ.spread_index, occ.forced_page))
                .is_none_or(|old| old.item != occ.item || old.parent != occ.parent || old.bounds != occ.bounds)
        })
        .collect();
    let mut issues = Vec::new();
    for (i, frame) in after.iter().enumerate().filter(|(i, _)| changed.get(*i) == Some(&true)) {
        let b = frame.bounds;
        let m = frame.safe;
        if b.x0 < m.x0 - TOLERANCE || b.y0 < m.y0 - TOLERANCE || b.x1 > m.x1 + TOLERANCE || b.y1 > m.y1 + TOLERANCE {
            issues.push(json!({"kind":"textOutsideMargins","item":frame.item.id.0,"page":frame.page+1,
                "bounds":[b.x0,b.y0,b.x1,b.y1],"safeMargins":[m.x0,m.y0,m.x1,m.y1]}));
        }
        for (j, other) in after.iter().enumerate() {
            if i == j || frame.page_id != other.page_id || (changed.get(j) == Some(&true) && j < i) {
                continue;
            }
            let o = other.bounds;
            if b.x1.min(o.x1) - b.x0.max(o.x0) > TOLERANCE && b.y1.min(o.y1) - b.y0.max(o.y0) > TOLERANCE {
                issues.push(json!({"kind":"textOverlap","item":frame.item.id.0,"otherItem":other.item.id.0,"page":frame.page+1,
                    "bounds":[b.x0,b.y0,b.x1,b.y1],"otherBounds":[o.x0,o.y0,o.x1,o.y1]}));
            }
            if issues.len() >= 32 {
                break;
            }
        }
        if issues.len() >= 32 {
            break;
        }
    }
    if issues.len() < 32 {
        for (i, frame) in after.iter().enumerate() {
            let b = frame.bounds;
            let frame_area = b.width().max(0.0) * b.height().max(0.0);
            if frame_area <= TOLERANCE {
                continue;
            }
            let frame_is_changed = changed.get(i) == Some(&true);
            let old_frame = previous.get(&(frame.item.id, frame.page_id)).copied();
            for (k, occ) in after_occluders.iter().enumerate() {
                if occ.spread_index != frame.spread_index || occ.paint_order <= frame.paint_order {
                    continue;
                }
                // Container frames with pasted-into children draw their fill behind nested children.
                if occ.loc.spread == frame.loc.spread && frame.loc.path.starts_with(&occ.loc.path) {
                    continue;
                }
                let occ_is_changed = occluder_changed.get(k) == Some(&true);
                let preexisting_above = !frame_is_changed
                    && !occ_is_changed
                    && old_frame
                        .zip(previous_occluders.get(&(occ.item.id, occ.spread_index, occ.forced_page)).copied())
                        .is_some_and(|(prev_f, prev_o)| prev_o.paint_order > prev_f.paint_order);
                if preexisting_above {
                    continue;
                }
                let o = occ.bounds;
                let ix = (b.x1.min(o.x1) - b.x0.max(o.x0)).max(0.0);
                let iy = (b.y1.min(o.y1) - b.y0.max(o.y0)).max(0.0);
                if ix > MIN_OCCLUSION_DIM_PT && iy > MIN_OCCLUSION_DIM_PT && ix * iy >= MIN_OCCLUSION_RATIO * frame_area {
                    issues.push(json!({
                        "kind": "textOccludedByObject",
                        "item": frame.item.id.0,
                        "occludingItem": occ.item.id.0,
                        "page": frame.page + 1,
                        "bounds": [b.x0, b.y0, b.x1, b.y1],
                        "occluderBounds": [o.x0, o.y0, o.x1, o.y1],
                    }));
                    if issues.len() >= 32 {
                        break;
                    }
                }
            }
            if issues.len() >= 32 {
                break;
            }
        }
    }
    if !issues.is_empty() {
        return Err(bad(ID, format!("text layout rejected draft: {}", Value::Array(issues))));
    }
    Ok(())
}

struct CheckedRaster {
    sha256: [u8; 32],
    bytes: usize,
    pixels: u64,
}

fn check_image(p: &Value) -> Result<CheckedRaster> {
    if p.get("frame").is_none() && !["x", "y", "width"].iter().all(|key| p.get(key).is_some()) {
        return Err(bad(ID, "image placement needs an explicit frame or x, y and width; loading the place cursor is not permitted"));
    }
    let encoded = p.get("base64").and_then(Value::as_str).ok_or_else(|| bad(ID, "inline image required"))?;
    if encoded.len() > MAX_ENCODED_IMAGE_BYTES {
        return Err(bad(ID, "inline image exceeds the 16 MiB encoded raster allowance"));
    }
    let name = p.get("name").and_then(Value::as_str).ok_or_else(|| bad(ID, "image name required"))?.to_ascii_lowercase();
    let bytes = super::base64_decode(encoded);
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(bad(ID, "inline image exceeds 16 MiB of raster bytes"));
    }
    let png = bytes.starts_with(b"\x89PNG\r\n\x1a\n") && name.ends_with(".png");
    let jpeg = bytes.starts_with(&[0xff, 0xd8, 0xff]) && (name.ends_with(".jpg") || name.ends_with(".jpeg"));
    if !png && !jpeg {
        return Err(bad(ID, "only inline PNG/JPEG image bytes are permitted; SVG, PDF, paths and media are excluded"));
    }
    // Header inspection does not decode pixel data. Refuse PNG/JPEG decompression bombs first.
    let (w, h) = designcraft_images::pixel_size(&bytes).ok_or_else(|| bad(ID, "invalid raster image header"))?;
    let pixels = u64::from(w) * u64::from(h);
    if w == 0 || h == 0 || pixels > MAX_IMAGE_PIXELS {
        return Err(bad(ID, "image dimensions exceed 20 million pixels"));
    }
    Ok(CheckedRaster { sha256: Sha256::digest(&bytes).into(), bytes: bytes.len(), pixels })
}

/// Validate and simulate a complete production against the current document without editing it.
pub fn validate_actions(s: &Session, p: &Value) -> Result<Value> {
    let (_, results) = draft(s, p, || false)?;
    Ok(json!({"valid":true,"steps":results.len(),"results":results,"documentUid":s.doc()?.uid,"revision":s.doc()?.revision}))
}

/// Atomic publication, with cooperative cancellation checked between actions and before commit.
/// This public helper also records exactly one undo entry when called outside the registry.
pub fn apply_actions(s: &mut Session, p: &Value, cancelled: impl FnMut() -> bool) -> Result<Value> {
    let (finished, results) = draft(s, p, cancelled)?;
    let label = p.get("label").and_then(Value::as_str).unwrap_or("AI Production");
    publish_draft(s, finished, label)?;
    Ok(json!({"applied":true,"steps":results.len(),"results":results,"label":label,"documentUid":s.doc()?.uid,"revision":s.doc()?.revision}))
}

fn publish_draft(s: &mut Session, mut finished: crate::DocState, label: &str) -> Result<()> {
    let original = s.doc()?;
    finished.history = original.history.clone();
    if !std::sync::Arc::ptr_eq(&finished.doc, &original.doc) {
        crate::record_undo(
            &mut finished,
            HistoryEntry { label: label.to_string(), doc: original.doc.clone(), selection: original.selection.clone() },
        );
    }
    *s.doc_mut()? = finished;
    s.typing_format = None;
    s.reset_find_scope();
    Ok(())
}

/// Publish a human review group atomically. Every chunk retains the single-plan bounds and
/// capabilities, including chunk-local `$N` references; no intermediate draft reaches `s`.
/// Cancellation or a later chunk's error leaves the live document and its history untouched.
pub fn apply_action_groups(s: &mut Session, p: &Value, mut cancelled: impl FnMut() -> bool) -> Result<Value> {
    let params = p.as_object().ok_or_else(|| bad(GROUP_ID, "parameters must be an object"))?;
    for key in params.keys() {
        if !["chunks", "outputMode", "label", "expectedDocumentUid", "expectedRevision", "requirePreflight", "minPpi", "documentSetup"]
            .contains(&key.as_str())
        {
            return Err(bad(GROUP_ID, format!("unknown parameter `{key}`")));
        }
    }
    let chunks = p
        .get("chunks")
        .and_then(Value::as_array)
        .filter(|chunks| !chunks.is_empty() && chunks.len() <= MAX_GROUPS)
        .ok_or_else(|| bad(GROUP_ID, "chunks must contain 1 to 1000 plans"))?;
    // Two outer levels enclose the same per-plan shape: `chunks` and each chunk object.
    bounded_limits(p, GROUP_ID, MAX_GROUP_BYTES, MAX_GROUPS * 65536, 26)?;
    if let Some(label) = p.get("label")
        && !label.as_str().is_some_and(|s| !s.trim().is_empty() && s.len() <= 256)
    {
        return Err(bad(GROUP_ID, "label must contain 1 to 256 bytes"));
    }
    // This host envelope is deliberately absent from the provider command schemas. A review
    // group may change pagination/binding atomically, but cannot smuggle size/page-count or
    // unrelated Document Setup changes through the host.
    let document_setup = if let Some(value) = p.get("documentSetup") {
        let setup = value.as_object().ok_or_else(|| bad(GROUP_ID, "documentSetup must be an object"))?;
        for (key, value) in setup {
            match key.as_str() {
                "facingPages" if value.as_bool().is_some() => {}
                "binding" if matches!(value.as_str(), Some("leftToRight" | "rightToLeft")) => {}
                "facingPages" => return Err(bad(GROUP_ID, "documentSetup.facingPages must be a boolean")),
                "binding" => return Err(bad(GROUP_ID, "documentSetup.binding must be leftToRight or rightToLeft")),
                _ => return Err(bad(GROUP_ID, format!("documentSetup: unknown parameter `{key}`"))),
            }
        }
        Some(value)
    } else {
        None
    };
    let original = s.doc()?;
    if original.interaction.is_some() || original.preview_stash.is_some() || s.tool.composing() {
        return Err(bad(GROUP_ID, "finish the current interaction or preview first"));
    }
    for (key, actual) in [("expectedDocumentUid", original.uid), ("expectedRevision", original.revision)] {
        if let Some(v) = p.get(key)
            && v.as_u64() != Some(actual)
        {
            return Err(bad(GROUP_ID, format!("{key}: document changed while generation was pending")));
        }
    }
    let original_page_count = original.doc.page_count();
    let mut work = Session::new();
    work.cache = s.cache.clone();
    work.prefs = s.prefs.clone();
    work.docs.push(original.clone());
    work.active = Some(0);
    work.in_undo_step = true;
    if let Some(setup) = document_setup {
        if cancelled() {
            return Err(bad(GROUP_ID, "production cancelled"));
        }
        work.execute("layout.documentSetup", setup)?;
        if work.doc()?.doc.page_count() != original_page_count {
            return Err(bad(GROUP_ID, "documentSetup cannot add or remove existing document pages"));
        }
    }
    let mut results = Vec::with_capacity(chunks.len());
    let mut steps = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        if cancelled() {
            return Err(bad(GROUP_ID, "production cancelled"));
        }
        bounded(chunk)?;
        let chunk = chunk.as_object().ok_or_else(|| bad(GROUP_ID, format!("chunk {index}: expected an object")))?;
        if let Some(key) = chunk.keys().find(|key| key.as_str() != "commands" && key.as_str() != "label") {
            return Err(bad(GROUP_ID, format!("chunk {index}: unknown parameter `{key}`")));
        }
        let mut chunk_params = serde_json::Map::new();
        for key in ["outputMode", "requirePreflight", "minPpi"] {
            if let Some(value) = p.get(key) {
                chunk_params.insert(key.into(), value.clone());
            }
        }
        chunk_params.extend(chunk.clone());
        let (finished, chunk_results) = draft(&work, &Value::Object(chunk_params), &mut cancelled)?;
        *work.doc_mut()? = finished;
        steps = steps.saturating_add(chunk_results.len());
        results.push(chunk_results);
    }
    if cancelled() {
        return Err(bad(GROUP_ID, "production cancelled"));
    }
    let label = p.get("label").and_then(Value::as_str).unwrap_or("AI Production Group");
    publish_draft(s, work.doc()?.clone(), label)?;
    Ok(
        json!({"applied":true,"groups":results.len(),"steps":steps,"results":results,"label":label,"documentUid":s.doc()?.uid,"revision":s.doc()?.revision}),
    )
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "production.catalog","AI Production Commands",[],None,"{} → vetted commands with paramsSchema and examples",always,|_,_| Ok(catalog())),
        cmd!(query "production.validate","Validate AI Production",[],None,"{commands: [{command, params}], outputMode, allowedCommands?, label?, expectedDocumentUid?, expectedRevision?, requirePreflight?: bool (default false; refuse error-level issues before publication), minPpi?: 150 (1 to 2400)} → {valid, steps, results}",has_doc,|s,p| validate_actions(s,p)),
        cmd!(noundo "production.apply","Apply AI Production",[],None,"{commands: [{command, params}], outputMode, allowedCommands?, label?, expectedDocumentUid?, expectedRevision?, requirePreflight?: bool (default false; refuse error-level issues before publication), minPpi?: 150 (1 to 2400)} → {applied, steps, results}",has_doc,|s,p| apply_actions(s,p,|| false)),
        cmd!(noundo "production.applyGroup","Apply AI Production Group",[],None,"{chunks: [{commands: [{command, params}], label?}] (1 to 1000 chunks, at most 512 commands each; references reset per chunk), outputMode, label?, expectedDocumentUid?, expectedRevision?, requirePreflight?: bool, minPpi?: 150 (1 to 2400), documentSetup?: {facingPages?: bool, binding?: leftToRight|rightToLeft} (host only; applied privately before chunks without changing page count)} → {applied, groups, steps, results: [chunk results], label, documentUid, revision}",has_doc,|s,p| apply_action_groups(s,p,|| false)),
    ]
}

#[cfg(test)]
mod tests {
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
    use super::*;
    use std::sync::Arc;

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
        // The host can validate identical mapped chunks against a private formatted state
        // before any setup or new artwork reaches the live document.
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
        assert_eq!(
            after.doc.item(crate::doc::ItemId(cover["id"].as_u64().unwrap())),
            before.doc.item(crate::doc::ItemId(cover["id"].as_u64().unwrap()))
        );
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

    // A complete grayscale PNG with stored DEFLATE blocks. The ancillary chunk
    // models a large encoded provider asset without allocating a full RGBA raster.
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
        for (rects, kind) in [
            (vec![[72.0, 72.0, 250.0, 150.0], [180.0, 100.0, 300.0, 180.0]], "textOverlap"),
            (vec![[20.0, 72.0, 150.0, 120.0]], "textOutsideMargins"),
        ] {
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

        // Reject empty find string, unknown dropShadow swatch, or unknown table.setCell fill swatch atomically.
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
        // Graphic duplication via stepAndRepeat must be rejected.
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        let step_repeat_graphic = json!({"outputMode":"native_with_images","commands":[
            {"command":"frame.create","params":{"rect":[54,54,154,154],"content":"graphic"}},
            {"command":"file.place","params":{"frame":"$0.id","name":"illustration.png","base64":png}},
            {"command":"edit.stepAndRepeat","params":{"ids":["$0.id"],"count":2,"dx":40,"dy":0}}
        ]});
        assert!(s.execute("production.apply", &step_repeat_graphic).is_err());

        // Invalid QR code rectangle or unknown swatch must be rejected atomically.
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
        // Background card painted BEFORE (beneath) text is valid.
        let valid_bg = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
            {"command":"frame.create","params":{"rect":[54,54,260,120],"content":"unassigned"}},
            {"command":"object.fill","params":{"ids":["$0.id"],"swatch":"[Paper]"}},
            {"command":"frame.create","params":{"rect":[62,62,260,110],"content":"text","text":"ABBA","caret":false}},
            {"command":"type.char","params":{"attrs":{"fontFamily":font}}},
            {"command":"line.create","params":{"a":[54,85],"b":[260,85]}}
        ]});
        s.execute("production.apply", &valid_bg).unwrap();

        let before = s.doc().unwrap().doc.clone();
        // Opaque rectangle created AFTER (above) text covering >=35% of text bounds is rejected.
        let occluding_rect = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
            {"command":"frame.create","params":{"rect":[60,60,240,108],"content":"unassigned"}},
            {"command":"object.fill","params":{"ids":["$0.id"],"swatch":"[Paper]"}}
        ]});
        let err = s.execute("production.apply", &occluding_rect).unwrap_err();
        assert!(err.to_string().contains("textOccludedByObject"), "expected textOccludedByObject, got: {err}");
        assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));

        // Bringing the existing background card to the front over the text is also rejected.
        let bg_id = s.doc().unwrap().doc.spreads[0].items[0].id.0;
        let bring_bg_to_front = json!({"outputMode":"native_only","requirePreflight":true,"commands":[
            {"command":"object.arrange","params":{"ids":[bg_id],"to":"front"}}
        ]});
        let err_arrange = s.execute("production.apply", &bring_bg_to_front).unwrap_err();
        assert!(err_arrange.to_string().contains("textOccludedByObject"), "expected textOccludedByObject on arrange, got: {err_arrange}");
        assert!(Arc::ptr_eq(&before, &s.doc().unwrap().doc));
    }
}
