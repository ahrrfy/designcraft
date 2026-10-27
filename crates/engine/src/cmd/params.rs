//! The parameter keys a command accepts, read from its params description (#776).
//!
//! A description starts with the params object: `{x, width?: pt, name?|title?: …, sides=6, …}`, or
//! with the objects a command takes instead of one another: `{path} or {name, dataBase64}`,
//! `{name} | {names: […]}`. Each entry of an object starts with a key, or with alternatives split
//! by `|`; a `?` marks an optional one, and a `:`, `=` or `(` starts what the entry says about it.
//! An entry that starts with `…` stands for keys listed elsewhere: those of each command it names
//! after `as` or `of` (`…settings as document.new`), or, naming none, any key. Objects inside a
//! value aren't described key by key, so only the top level is checked.
//!
//! Callers outside the app (the control channel, MCP, the CLI) are answered with a parameter error
//! for a key the description doesn't name ([`check_params`]): a misspelt `widht` no longer runs the
//! command with its default. [`Session::execute`](crate::Session::execute) itself takes what it is
//! given, so the journal of an older version still replays.
//!
//! Ported from VectorCraft (storytold/vectorcraft#1442).

use serde_json::Value;

use super::{bad, find_command};
use crate::Result;

/// What a params description says about the keys of the params object.
#[derive(Debug, Default, PartialEq)]
pub struct ParamKeys<'a> {
    /// The keys it names.
    pub keys: Vec<&'a str>,
    /// Its `…` entries, without the `…`.
    pub more: Vec<&'a str>,
}

/// The text between the braces of the object `doc` starts with and what follows it, or why it
/// has none.
fn object_body(doc: &str) -> std::result::Result<(&str, &str), String> {
    // (Named, not written in the `Err`: these are for the tests of the descriptions, not messages
    // the app shows, which the message catalogs would have to translate.)
    let (no_object, not_closed) = ("the description doesn't start with the params object `{…}`", "the params object `{…}` isn't closed");
    if !doc.starts_with('{') {
        return Err(no_object.into());
    }
    let (mut depth, mut quote) = (0usize, None);
    for (i, c) in doc.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '`') => quote = Some(c),
            (None, '{' | '[' | '(') => depth += 1,
            (None, '}' | ']' | ')') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok((doc.get(1..i).unwrap_or_default(), doc.get(i + 1..).unwrap_or_default()));
                }
            }
            _ => {}
        }
    }
    Err(not_closed.into())
}

/// `s` split at each `sep` that is outside brackets, quotes and backticks.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let (mut out, mut depth, mut quote, mut start) = (Vec::new(), 0usize, None, 0);
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '`') => quote = Some(c),
            (None, '{' | '[' | '(') => depth += 1,
            (None, '}' | ']' | ')') => depth = depth.saturating_sub(1),
            (None, _) if c == sep && depth == 0 => {
                out.push(s.get(start..i).unwrap_or_default());
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(s.get(start..).unwrap_or_default());
    out
}

fn is_key(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The keys a params description names, or what in it doesn't read as one.
pub fn doc_keys(doc: &str) -> std::result::Result<ParamKeys<'_>, String> {
    let mut out = ParamKeys::default();
    let (mut body, mut rest) = object_body(doc)?;
    let mut entries = split_top(body, ',');
    // `{…} or {…}`, `{…} | {…}`: the objects the command takes instead.
    while let Some(next) = rest.trim_start().strip_prefix("or ").or_else(|| rest.trim_start().strip_prefix("| ")).filter(|r| r.starts_with('{')) {
        (body, rest) = object_body(next)?;
        entries.extend(split_top(body, ','));
    }
    for entry in entries.into_iter().map(str::trim).filter(|e| !e.is_empty()) {
        if let Some(more) = entry.strip_prefix('…') {
            out.more.push(more.trim());
            continue;
        }
        // `a?|b?: note` names two keys, and so does `a?: note | b?: note`; in `a?: x|y` the `y`
        // is a value.
        let mut noted = false;
        for alternative in split_top(entry, '|') {
            let head = alternative.split([':', '=', '(']).next().unwrap_or_default();
            let rest = alternative.get(head.len()..).unwrap_or_default();
            let name = head.trim().trim_end_matches('?').trim_end();
            if !noted {
                if !is_key(name) {
                    let start: String = entry.chars().take(60).collect();
                    let why = format!("`{start}` doesn't start with a parameter name");
                    return Err(why);
                }
                out.keys.push(name);
                noted = !rest.is_empty();
            } else if is_key(name) && (head.trim_end().ends_with('?') || rest.starts_with(':')) {
                out.keys.push(name);
            }
        }
    }
    Ok(out)
}

/// The commands whose keys a `…` entry stands for: those it names after `as` or `of` (`settings as
/// file.new`, `the PDF options of document.exportPdf`), not one it only points to (`see …`).
fn named_commands(more: &str) -> impl Iterator<Item = &'static super::CommandSpec> + '_ {
    let words: Vec<&str> = more.split_whitespace().collect();
    let id = |w: &str| find_command(w.trim_matches(|c: char| !c.is_ascii_alphanumeric()));
    (1..words.len()).filter_map(move |i| id(words.get(i)?).filter(|_| matches!(words.get(i - 1), Some(&"as" | &"of"))))
}

/// Every key command `doc` accepts, the keys of the commands its `…` entries name included.
/// `None`: any key (a `…` entry naming no command, or a description that doesn't parse, which
/// the tests rule out).
fn accepted(doc: &'static str, depth: usize) -> Option<Vec<&'static str>> {
    let parsed = doc_keys(doc).ok()?;
    let mut keys = parsed.keys;
    // A command that acts on `ids` takes one object as `id` too (see `cmd::targets`).
    if keys.contains(&"ids") && !keys.contains(&"id") {
        keys.push("id");
    }
    for more in parsed.more {
        let mut named = named_commands(more).peekable();
        // A reference that leads back here would never end: descriptions refer one or two deep.
        if named.peek().is_none() || depth > 4 {
            return None;
        }
        for spec in named {
            // A command that isn't checked yet doesn't vouch for the ones that take its keys.
            if UNCHECKED.contains(&spec.id) {
                return None;
            }
            keys.extend(accepted(spec.params, depth + 1)?);
        }
    }
    Some(keys)
}

/// The edits (insert, delete, replace, swap of neighbours) that turn `a` into `b`, ignoring case.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().map(|c| c.to_ascii_lowercase()).collect(), b.chars().map(|c| c.to_ascii_lowercase()).collect());
    let (mut before, mut prev): (Vec<usize>, Vec<usize>) = (Vec::new(), (0..=b.len()).collect());
    for (i, ca) in a.iter().enumerate() {
        let mut row = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let at = |r: &[usize], k: usize| r.get(k).copied().unwrap_or(usize::MAX / 2);
            let mut d = (at(&prev, j) + usize::from(ca != cb)).min(at(&prev, j + 1) + 1).min(at(&row, j) + 1);
            if i > 0 && j > 0 && a.get(i - 1) == Some(cb) && b.get(j - 1) == Some(ca) {
                d = d.min(at(&before, j - 1) + 1);
            }
            row.push(d);
        }
        before = std::mem::replace(&mut prev, row);
    }
    prev.last().copied().unwrap_or(0)
}

/// The key of `keys` a misspelt `key` most likely meant: the nearest one, when it is near.
fn nearest<'a>(key: &str, keys: &[&'a str]) -> Option<&'a str> {
    let near = keys.iter().map(|k| (distance(key, k), *k)).min()?;
    (near.0 <= (key.chars().count() / 3).max(1)).then_some(near.1)
}

/// `params` has only keys the description `doc` of command `id` names, or the parameter error
/// naming the first one it doesn't, with the nearest accepted key or the accepted ones.
pub fn check_keys(id: &str, doc: &'static str, params: &Value) -> Result<()> {
    let Value::Object(given) = params else { return Ok(()) };
    if given.is_empty() {
        return Ok(());
    }
    let Some(keys) = accepted(doc, 0) else { return Ok(()) };
    match given.keys().find(|k| !keys.contains(&k.as_str())) {
        Some(key) => Err(unknown(id, key, &keys)),
        None => Ok(()),
    }
}

/// The parameter error of command `id` for `key`, which isn't one of `keys`: with the nearest of
/// them, or all of them.
fn unknown(id: &str, key: &str, keys: &[&str]) -> crate::EngineError {
    bad(
        id,
        match nearest(key, keys) {
            Some(near) => format!("unknown parameter `{key}`; did you mean `{near}`?"),
            None if keys.is_empty() => format!("unknown parameter `{key}`; the command takes no parameters"),
            None => format!("unknown parameter `{key}`; the command takes: {}", keys.join(", ")),
        },
    )
}

/// Commands whose params aren't checked yet (#776): they read keys in ways the audit of the
/// descriptions couldn't follow (a list of keys in a loop, a whole object read as settings), so
/// their descriptions aren't proven to name every key they take. Sorted; take a command out once
/// its description is checked against its code.
pub const UNCHECKED: &[&str] = &[
    // Take the attributes under `attrs` or as the params object itself; the attribute setters
    // refuse a key they don't know.
    "type.char",
    "type.para",
];

/// Structured metadata for one top-level parameter parsed from `CommandSpec::params`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandParamFieldSpec {
    /// Parameter key name (without trailing `?`).
    pub name: String,
    /// True when marked with `?` or absent in an alternative `{…} or {…}` object variant.
    pub optional: bool,
    /// Closed list of string literal values when unambiguously documented as `a|b|…`.
    pub choices: Vec<String>,
    /// True when unambiguously documented as a boolean parameter (`bool`, `true|false`, `=true`, `=false`).
    pub boolean: bool,
    /// Documented value hint / note (trimmed, without leading `:`).
    pub hint: String,
}

const NON_CHOICE_TYPE_WORDS: &[&str] = &[
    "number", "int", "integer", "float", "u64", "i64", "usize", "f64", "pt", "px", "mm", "cm", "em", "n", "bool", "boolean", "string", "str",
    "object", "array", "null", "any", "id", "ids", "index", "count", "name", "names", "swatch", "hex", "set", "ssNN",
];

fn strip_trailing_paren_note(s: &str) -> &str {
    let trimmed = s.trim();
    if !trimmed.ends_with(')') {
        return trimmed;
    }
    let mut depth = 0usize;
    let mut quote = None;
    let mut open_idx = None;
    for (i, c) in trimmed.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '`') => quote = Some(c),
            (None, '(') => {
                if depth == 0 {
                    open_idx = Some(i);
                }
                depth += 1;
            }
            (None, ')') => {
                depth = depth.saturating_sub(1);
                if depth == 0 && i + c.len_utf8() < trimmed.len() {
                    open_idx = None;
                }
            }
            _ => {}
        }
    }
    match open_idx {
        Some(0) | None => trimmed,
        Some(idx) => trimmed.get(..idx).unwrap_or(trimmed).trim(),
    }
}

fn is_choice_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let ok =
        chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') && !s.is_empty();
    ok && !NON_CHOICE_TYPE_WORDS.contains(&s) && !matches!(s, "true" | "false")
}

fn clean_hint_text(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_colon = trimmed.strip_prefix(':').map_or(trimmed, str::trim);
    without_colon.to_string()
}

fn classify_hint(hint: &str) -> (bool, Vec<String>) {
    let core = strip_trailing_paren_note(hint);
    let core_no_eq = core.strip_prefix('=').map_or(core, str::trim);
    if matches!(core_no_eq, "bool" | "boolean" | "true|false" | "false|true" | "true" | "false")
        || core.starts_with("bool ")
        || core.starts_with("bool=")
    {
        return (true, Vec::new());
    }
    let alts = split_top(core_no_eq, '|');
    if alts.len() >= 2 {
        let mut choices = Vec::with_capacity(alts.len());
        for alt in alts {
            let token = strip_trailing_paren_note(alt);
            if !is_choice_ident(token) {
                return (false, Vec::new());
            }
            if !choices.iter().any(|existing| existing == token) {
                choices.push(token.to_string());
            }
        }
        if choices.len() >= 2 {
            return (false, choices);
        }
    }
    (false, Vec::new())
}

fn merge_field_spec(existing: &mut CommandParamFieldSpec, incoming: CommandParamFieldSpec) {
    existing.optional |= incoming.optional;
    if existing.boolean != incoming.boolean {
        existing.boolean = false;
    }
    let incoming_single = strip_trailing_paren_note(&incoming.hint);
    if !existing.choices.is_empty() {
        if !incoming.choices.is_empty() {
            for choice in incoming.choices {
                if !existing.choices.contains(&choice) {
                    existing.choices.push(choice);
                }
            }
        } else if is_choice_ident(incoming_single) {
            if !existing.choices.iter().any(|c| c == incoming_single) {
                existing.choices.push(incoming_single.to_string());
            }
        } else {
            existing.choices.clear();
        }
    }
    if existing.hint.is_empty() && !incoming.hint.is_empty() {
        existing.hint = incoming.hint;
    }
}

/// Parse structured parameter field metadata from a `CommandSpec::params` description.
pub fn parse_params_doc(doc: &str) -> std::result::Result<Vec<CommandParamFieldSpec>, String> {
    let (mut body, mut rest) = object_body(doc)?;
    let mut variant_bodies = vec![body];
    while let Some(next) = rest.trim_start().strip_prefix("or ").or_else(|| rest.trim_start().strip_prefix("| ")).filter(|r| r.starts_with('{')) {
        (body, rest) = object_body(next)?;
        variant_bodies.push(body);
    }
    let multi_variant = variant_bodies.len() > 1;
    let mut fields: Vec<CommandParamFieldSpec> = Vec::new();
    let mut variant_key_sets: Vec<Vec<String>> = Vec::with_capacity(variant_bodies.len());

    for vbody in variant_bodies {
        let mut variant_keys = Vec::new();
        for entry in split_top(vbody, ',').into_iter().map(str::trim).filter(|e| !e.is_empty()) {
            if entry.starts_with('…') {
                continue;
            }
            let mut noted = false;
            let mut pending: Vec<(String, bool)> = Vec::new();
            let mut raw_hint = String::new();
            let flush = |pending: &mut Vec<(String, bool)>,
                         raw_hint: &mut String,
                         fields: &mut Vec<CommandParamFieldSpec>,
                         variant_keys: &mut Vec<String>| {
                if pending.is_empty() {
                    return;
                }
                let hint = clean_hint_text(raw_hint);
                let (boolean, choices) = classify_hint(&hint);
                for (name, optional) in pending.drain(..) {
                    if !variant_keys.contains(&name) {
                        variant_keys.push(name.clone());
                    }
                    let spec = CommandParamFieldSpec { name, optional, choices: choices.clone(), boolean, hint: hint.clone() };
                    if let Some(prev) = fields.iter_mut().find(|f| f.name == spec.name) {
                        merge_field_spec(prev, spec);
                    } else {
                        fields.push(spec);
                    }
                }
                raw_hint.clear();
            };

            for alternative in split_top(entry, '|') {
                let head = alternative.split([':', '=', '(']).next().unwrap_or_default();
                let tail = alternative.get(head.len()..).unwrap_or_default();
                let optional = head.trim_end().ends_with('?');
                let name = head.trim().trim_end_matches('?').trim_end();
                if !noted {
                    if !is_key(name) {
                        let start: String = entry.chars().take(60).collect();
                        return Err(format!("`{start}` doesn't start with a parameter name"));
                    }
                    pending.push((name.to_string(), optional));
                    if !tail.is_empty() {
                        raw_hint.push_str(tail);
                        noted = true;
                    }
                } else if is_key(name) && (optional || tail.starts_with(':')) {
                    flush(&mut pending, &mut raw_hint, &mut fields, &mut variant_keys);
                    pending.push((name.to_string(), optional));
                    if !tail.is_empty() {
                        raw_hint.push_str(tail);
                        noted = true;
                    } else {
                        noted = false;
                    }
                } else {
                    raw_hint.push('|');
                    raw_hint.push_str(alternative.trim());
                }
            }
            flush(&mut pending, &mut raw_hint, &mut fields, &mut variant_keys);
        }
        variant_key_sets.push(variant_keys);
    }

    if multi_variant {
        for field in &mut fields {
            if !variant_key_sets.iter().all(|vk| vk.contains(&field.name)) {
                field.optional = true;
            }
        }
    }
    Ok(fields)
}

fn parse_numeric_range(core: &str) -> Option<(f64, f64)> {
    let (lhs, rhs) = core.split_once("..").or_else(|| core.split_once('–'))?;
    let min = lhs.trim().parse::<f64>().ok()?;
    let max = rhs.trim().parse::<f64>().ok()?;
    (min.is_finite() && max.is_finite() && min <= max).then_some((min, max))
}

fn normalize_compact(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn with_description(mut schema: Value, hint: &str) -> Value {
    if !hint.is_empty()
        && let Some(obj) = schema.as_object_mut()
    {
        obj.insert("description".to_string(), Value::String(hint.to_string()));
    }
    schema
}

fn infer_field_json_schema(field: &CommandParamFieldSpec) -> Value {
    let hint = field.hint.trim();
    if field.name == "ids" {
        return with_description(
            serde_json::json!({
                "type": "array",
                "items": {
                    "anyOf": [
                        {"type": "integer", "minimum": 0},
                        {"type": "string", "minLength": 1}
                    ]
                },
                "maxItems": 1024
            }),
            hint,
        );
    }
    if field.boolean {
        return with_description(serde_json::json!({"type": "boolean"}), hint);
    }
    if !field.choices.is_empty() {
        return with_description(
            serde_json::json!({
                "type": "string",
                "enum": field.choices
            }),
            hint,
        );
    }

    let core = strip_trailing_paren_note(hint);
    let compact = normalize_compact(core);

    if matches!(compact.as_str(), "bool|null" | "null|bool" | "boolean|null" | "null|boolean") {
        return with_description(
            serde_json::json!({
                "anyOf": [{"type": "boolean"}, {"type": "null"}]
            }),
            hint,
        );
    }
    if matches!(compact.as_str(), "[x,y]" | "[dx,dy]" | "[a,b]" | "[start,end]" | "[h,v]") {
        return with_description(
            serde_json::json!({
                "type": "array",
                "items": {"type": "number"},
                "minItems": 2,
                "maxItems": 2
            }),
            hint,
        );
    }
    if matches!(
        compact.as_str(),
        "[x0,y0,x1,y1]" | "[left,top,right,bottom]" | "[top,bottom,inside,outside]" | "[t,l,b,r]" | "[top,left,bottom,right]"
    ) {
        return with_description(
            serde_json::json!({
                "type": "array",
                "items": {"type": "number"},
                "minItems": 4,
                "maxItems": 4
            }),
            hint,
        );
    }
    if matches!(
        compact.as_str(),
        "n|[t,l,b,r]" | "number|[t,l,b,r]" | "number|[top,left,bottom,right]" | "[top,left,bottom,right]|number" | "n|[top,bottom,inside,outside]"
    ) {
        return with_description(
            serde_json::json!({
                "anyOf": [
                    {"type": "number"},
                    {"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4}
                ]
            }),
            hint,
        );
    }

    let alts = split_top(core, '|');
    if alts.len() >= 2 {
        let mut choice_tokens = Vec::new();
        let mut saw_number = false;
        let mut all_understood = true;
        for alt in &alts {
            let tok = strip_trailing_paren_note(alt);
            if matches!(tok, "number" | "n" | "pt") {
                saw_number = true;
            } else if is_choice_ident(tok) {
                choice_tokens.push(tok.to_string());
            } else {
                all_understood = false;
                break;
            }
        }
        if all_understood && saw_number && !choice_tokens.is_empty() {
            return with_description(
                serde_json::json!({
                    "anyOf": [
                        {"type": "string", "enum": choice_tokens},
                        {"type": "number"}
                    ]
                }),
                hint,
            );
        }
    }

    if alts.len() == 1 {
        if let Some((min, max)) = parse_numeric_range(core) {
            return with_description(
                serde_json::json!({
                    "type": "number",
                    "minimum": min,
                    "maximum": max
                }),
                hint,
            );
        }
        if core.starts_with('[') && core.ends_with(']') {
            return with_description(
                serde_json::json!({
                    "type": "array",
                    "items": {},
                    "maxItems": 1024
                }),
                hint,
            );
        }
        if core.starts_with('{') && core.ends_with('}') {
            return with_description(
                serde_json::json!({
                    "type": "object",
                    "additionalProperties": true
                }),
                hint,
            );
        }
    }

    if field.name == "id" && (hint.is_empty() || matches!(core, "id" | "u64")) {
        return with_description(
            serde_json::json!({
                "anyOf": [
                    {"type": "integer", "minimum": 0},
                    {"type": "string", "minLength": 1}
                ]
            }),
            hint,
        );
    }

    with_description(serde_json::json!({}), hint)
}

fn collect_fields(doc: &'static str, depth: usize, out: &mut Vec<CommandParamFieldSpec>) -> Option<()> {
    let parsed_keys = doc_keys(doc).ok()?;
    let parsed_fields = parse_params_doc(doc).ok()?;
    for field in parsed_fields {
        if let Some(existing) = out.iter_mut().find(|f| f.name == field.name) {
            merge_field_spec(existing, field);
        } else {
            out.push(field);
        }
    }
    if out.iter().any(|f| f.name == "ids") && !out.iter().any(|f| f.name == "id") {
        out.push(CommandParamFieldSpec {
            name: "id".to_string(),
            optional: true,
            choices: Vec::new(),
            boolean: false,
            hint: "single target object id".to_string(),
        });
    }
    for more in parsed_keys.more {
        let mut named = named_commands(more).peekable();
        if named.peek().is_none() || depth > 4 {
            return None;
        }
        for spec in named {
            if UNCHECKED.contains(&spec.id) {
                return None;
            }
            collect_fields(spec.params, depth + 1, out)?;
        }
    }
    Some(())
}

/// Every top-level parameter key `spec` accepts according to its `CommandSpec::params` signature,
/// or `None` when the command accepts unchecked/open-ended keys.
pub fn accepted_keys(spec: &super::CommandSpec) -> Option<Vec<&'static str>> {
    if UNCHECKED.contains(&spec.id) {
        return None;
    }
    accepted(spec.params, 0)
}

/// Derive a JSON Schema object for `spec`'s accepted parameters directly from its
/// `CommandSpec::params` signature (including inherited `… as/of <cmd>` parameters).
pub fn command_params_json_schema(spec: &super::CommandSpec) -> Option<Value> {
    if UNCHECKED.contains(&spec.id) {
        return None;
    }
    let keys = accepted(spec.params, 0)?;
    let mut fields = Vec::new();
    collect_fields(spec.params, 0, &mut fields)?;
    let mut props = serde_json::Map::new();
    for key in keys {
        if props.contains_key(key) {
            continue;
        }
        let prop_schema = fields.iter().find(|f| f.name == key).map_or_else(|| serde_json::json!({}), infer_field_json_schema);
        props.insert(key.to_string(), prop_schema);
    }
    Some(serde_json::json!({
        "type": "object",
        "properties": props,
        "required": [],
        "additionalProperties": false,
    }))
}

/// [`check_keys`] for an engine command. A command the engine doesn't have passes: running it
/// reports that.
pub fn check_params(id: &str, params: &Value) -> Result<()> {
    let Some(spec) = find_command(id) else { return Ok(()) };
    if UNCHECKED.contains(&id) {
        return Ok(());
    }
    check_keys(id, spec.params, params)
}
