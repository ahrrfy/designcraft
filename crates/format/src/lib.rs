//! The native `.designcraft` format: a zip archive holding
//!
//! - `mimetype` (stored first, uncompressed): `application/vnd.designcraft+zip`
//! - `document.json`: the [`Document`] (pretty JSON, documented by the serde model in `designcraft-doc`)
//! - `assets/<id>.<ext>`: embedded placed files, byte-for-byte: one per distinct file, and only
//!   the ones the document references (see [`Document::compact_assets`])
//! - `meta.json`: format version and generator
//! - `preview.png` (optional): a small picture of the first page, for the start screen's recent files
//!
//! Older single-file JSON documents (assets as base64 in `assetData`) still open. Documents
//! from older format versions are upgraded on load so they lay out as they did.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::io::{Cursor, Read, Write};
use std::sync::Arc;

use designcraft_doc::{AssetId, Document, ParaAttrs, Story};
use serde_json::{Value, json};

pub const MIME: &str = "application/vnd.designcraft+zip";
/// Latest reader capability. Version 2 pinned the Keep Lines Together and drop cap defaults;
/// version 3 adds rendering of retained EPS source assets; version 4 is one capability covering both
/// retained placed-graphic layer options and explicit all-on placed-PDF layer choices, so a version 4
/// reader understands both. Any later capability needs version 5. [`required_version`] is the only
/// place that picks a version.
pub const VERSION: u32 = 4;

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("not a DesignCraft document: {0}")]
    NotOurs(String),
    #[error("document is from a newer DesignCraft (format {0}); please update")]
    TooNew(u64),
    #[error("{0}")]
    Io(String),
    /// The document holds something the file can't represent; nothing is written.
    #[error("{0}")]
    Unsaveable(String),
}

fn ext_for(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/tiff" => "tif",
        "application/pdf" => "pdf",
        "application/postscript" => "eps",
        "image/svg+xml" => "svg",
        _ => "bin",
    }
}

/// Serialize a document to `.designcraft` bytes. The file holds each distinct placed file once
/// and leaves out assets nothing references; `doc` itself keeps them (undo can bring them back).
pub fn save(doc: &Document) -> Result<Vec<u8>, FormatError> {
    save_with_preview(doc, None)
}

/// [`save`], plus `preview` (PNG bytes) as `preview.png`.
pub fn save_with_preview(doc: &Document, preview: Option<&[u8]>) -> Result<Vec<u8>, FormatError> {
    save_with_large_entries_from(doc, preview, LARGE_ENTRY)
}

/// Asset entries this size or larger are written with zip64 sizes. Zip rejects an entry over
/// 4 GiB written without them; the margin covers deflate output that is larger than its input.
const LARGE_ENTRY: usize = (u32::MAX as usize).saturating_sub(64 << 20);

/// The oldest reader capability that keeps everything in `doc`. Documents that need nothing newer
/// keep their older requirement so existing readers still open them.
fn required_version(doc: &Document) -> u32 {
    // Version 4 covers both of these: older readers ignore the unknown fields, so they would
    // silently drop retained graphic layer choices, or read an explicit all-on PDF layer choice
    // (empty hidden list) as "use the file's defaults". A nonempty hidden list stays readable.
    if doc.has_graphic_layer_options() || doc.has_explicit_all_on_pdf_layers() {
        return 4;
    }
    // Older readers would silently omit EPS artwork. Preview-only legacy assets and documents
    // without EPS stay at 2 so their existing readers keep working; writing 1 would make a
    // reader re-run the version 2 paragraph upgrade over current defaults.
    if doc.assets.values().any(|a| designcraft_images::is_eps(&a.data)) { 3 } else { 2 }
}

fn save_with_large_entries_from(doc: &Document, preview: Option<&[u8]>, large_entry: usize) -> Result<Vec<u8>, FormatError> {
    // JSON writes infinity and NaN as null, which no load accepts: the file would not open.
    if let Some(id) = doc.non_finite_item() {
        return Err(FormatError::Unsaveable(format!("object {} has a position or size that is not a finite number", id.0)));
    }
    let mut compact = doc.clone();
    compact.compact_assets();
    sanitize_for_save(&mut compact);
    let doc = &compact;
    let io = |e: zip::result::ZipError| FormatError::Io(e.to_string());
    let mut buf = Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        let deflate = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        z.start_file("mimetype", stored).map_err(io)?;
        z.write_all(MIME.as_bytes()).map_err(|e| FormatError::Io(e.to_string()))?;
        z.start_file("meta.json", deflate).map_err(io)?;
        let version = required_version(doc);
        let meta = json!({"format": "designcraft", "version": version, "generator": concat!("DesignCraft ", env!("CARGO_PKG_VERSION"))});
        z.write_all(meta.to_string().as_bytes()).map_err(|e| FormatError::Io(e.to_string()))?;
        if let Some(png) = preview {
            z.start_file(PREVIEW, stored).map_err(io)?;
            z.write_all(png).map_err(|e| FormatError::Io(e.to_string()))?;
        }
        z.start_file("document.json", deflate).map_err(io)?;
        let json = to_finite_json_vec_pretty(doc).map_err(|e| FormatError::Io(e.to_string()))?;
        z.write_all(&json).map_err(|e| FormatError::Io(e.to_string()))?;
        for (id, a) in &doc.assets {
            if a.data.is_empty() {
                continue;
            }
            // Already-compressed images are stored as-is.
            let opts = if matches!(a.mime.as_str(), "image/png" | "image/jpeg" | "image/gif" | "image/webp") { stored } else { deflate };
            let opts = opts.large_file(a.data.len() >= large_entry);
            z.start_file(format!("assets/{}.{}", id.0, ext_for(&a.mime)), opts).map_err(io)?;
            z.write_all(&a.data).map_err(|e| FormatError::Io(e.to_string()))?;
        }
        z.finish().map_err(io)?;
    }
    Ok(buf.into_inner())
}

fn sanitize_char_attrs(c: &mut designcraft_doc::CharAttrs) {
    if let Some(sz) = c.size
        && (!sz.is_finite() || sz <= 0.0)
    {
        c.size = Some(12.0);
    }
}

fn sanitize_para_attrs(p: &mut ParaAttrs) {
    if let Some(tabs) = p.tabs.take() {
        p.tabs = Some(designcraft_doc::TabStop::sanitized_list(tabs));
    }
}

fn story_has_invalid_save_attrs(st: &Story, depth: usize) -> bool {
    if depth > 32 {
        return false;
    }
    let bad_c = |c: &designcraft_doc::CharAttrs| c.size.is_some_and(|s| !s.is_finite() || s <= 0.0);
    let bad_p = |p: &ParaAttrs| p.tabs.as_ref().is_some_and(|t| t.iter().any(|s| !s.position.is_finite()));
    st.paras.iter().any(|p| bad_c(&p.chars) || bad_p(&p.para))
        || st.chars.iter().any(|r| bad_c(&r.format.over))
        || st.tables.values().any(|t| t.cells.iter().any(|cell| story_has_invalid_save_attrs(&cell.text, depth + 1)))
        || st.notes.iter().any(|n| story_has_invalid_save_attrs(&n.text, depth + 1))
        || st.endnotes.iter().any(|n| story_has_invalid_save_attrs(&n.text, depth + 1))
}

fn sanitize_styles_for_save(styles: &mut designcraft_doc::styles::Styles) {
    for ps in &mut styles.paragraph {
        sanitize_char_attrs(&mut ps.chars);
        sanitize_para_attrs(&mut ps.para);
    }
    for cs in &mut styles.character {
        sanitize_char_attrs(&mut cs.chars);
    }
    for cf in &mut styles.composite_fonts {
        for e in &mut cf.entries {
            if !e.relative_size.is_finite() || e.relative_size <= 0.0 {
                e.relative_size = 1.0;
            }
            if !e.horizontal_scale.is_finite() || e.horizontal_scale <= 0.0 {
                e.horizontal_scale = 1.0;
            }
            if !e.vertical_scale.is_finite() || e.vertical_scale <= 0.0 {
                e.vertical_scale = 1.0;
            }
            if !e.baseline_shift.is_finite() {
                e.baseline_shift = 0.0;
            }
        }
    }
    for mt in &mut styles.mojikumi_tables {
        for r in &mut mt.overrides {
            if !r.minimum.is_finite() {
                r.minimum = 0.0;
            }
            if !r.desired.is_finite() {
                r.desired = 0.0;
            }
            if !r.maximum.is_finite() {
                r.maximum = 0.0;
            }
        }
    }
}

/// Repair fields that have positive-finite invariants in [`Document::check`] or custom NaN-dropping
/// semantics (`TabStop`) before serialising, so a stray `NaN`/`Infinity` never writes an unopenable
/// `.designcraft` archive.
fn sanitize_for_save(doc: &mut Document) {
    sanitize_styles_for_save(doc.styles_mut());
    let mut fix_story = |st: &mut Story| {
        for p in &mut st.paras {
            sanitize_char_attrs(&mut p.chars);
            sanitize_para_attrs(&mut p.para);
        }
        for r in &mut st.chars {
            sanitize_char_attrs(&mut r.format.over);
        }
    };
    for st in doc.stories.values_mut() {
        if story_has_invalid_save_attrs(st, 0) {
            let st = Arc::make_mut(st);
            st.for_each_text_mut(&mut fix_story);
            for n in &mut st.endnotes {
                Arc::make_mut(n).text.for_each_text_mut(&mut fix_story);
            }
        }
    }
}

/// Pretty-print `value` to JSON while replacing any stray `NaN` or `±Infinity` `f32`/`f64` with
/// `0.0` instead of `null` (which `serde_json` emits for non-finite floats and later refuses to
/// deserialize into float fields).
fn to_finite_json_vec_pretty<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    use serde::ser::{
        Serialize, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple, SerializeTupleStruct, SerializeTupleVariant,
        Serializer,
    };

    struct Finite<'a, V: ?Sized>(&'a V);
    impl<V: ?Sized + Serialize> Serialize for Finite<'_, V> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            self.0.serialize(FiniteSer(serializer))
        }
    }

    struct FiniteSer<S>(S);
    struct Wrap<C>(C);

    impl<S: Serializer> Serializer for FiniteSer<S> {
        type Ok = S::Ok;
        type Error = S::Error;
        type SerializeSeq = Wrap<S::SerializeSeq>;
        type SerializeTuple = Wrap<S::SerializeTuple>;
        type SerializeTupleStruct = Wrap<S::SerializeTupleStruct>;
        type SerializeTupleVariant = Wrap<S::SerializeTupleVariant>;
        type SerializeMap = Wrap<S::SerializeMap>;
        type SerializeStruct = Wrap<S::SerializeStruct>;
        type SerializeStructVariant = Wrap<S::SerializeStructVariant>;

        fn serialize_bool(self, v: bool) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_bool(v)
        }
        fn serialize_i8(self, v: i8) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_i8(v)
        }
        fn serialize_i16(self, v: i16) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_i16(v)
        }
        fn serialize_i32(self, v: i32) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_i32(v)
        }
        fn serialize_i64(self, v: i64) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_i64(v)
        }
        fn serialize_i128(self, v: i128) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_i128(v)
        }
        fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_u8(v)
        }
        fn serialize_u16(self, v: u16) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_u16(v)
        }
        fn serialize_u32(self, v: u32) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_u32(v)
        }
        fn serialize_u64(self, v: u64) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_u64(v)
        }
        fn serialize_u128(self, v: u128) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_u128(v)
        }
        fn serialize_f32(self, v: f32) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_f32(if v.is_finite() { v } else { 0.0 })
        }
        fn serialize_f64(self, v: f64) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_f64(if v.is_finite() { v } else { 0.0 })
        }
        fn serialize_char(self, v: char) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_char(v)
        }
        fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_str(v)
        }
        fn serialize_bytes(self, v: &[u8]) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_bytes(v)
        }
        fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_none()
        }
        fn serialize_some<U: ?Sized + Serialize>(self, value: &U) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_some(&Finite(value))
        }
        fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_unit()
        }
        fn serialize_unit_struct(self, name: &'static str) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_unit_struct(name)
        }
        fn serialize_unit_variant(self, name: &'static str, idx: u32, variant: &'static str) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_unit_variant(name, idx, variant)
        }
        fn serialize_newtype_struct<U: ?Sized + Serialize>(self, name: &'static str, value: &U) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_newtype_struct(name, &Finite(value))
        }
        fn serialize_newtype_variant<U: ?Sized + Serialize>(
            self,
            name: &'static str,
            idx: u32,
            variant: &'static str,
            value: &U,
        ) -> Result<Self::Ok, Self::Error> {
            self.0.serialize_newtype_variant(name, idx, variant, &Finite(value))
        }
        fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
            self.0.serialize_seq(len).map(Wrap)
        }
        fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
            self.0.serialize_tuple(len).map(Wrap)
        }
        fn serialize_tuple_struct(self, name: &'static str, len: usize) -> Result<Self::SerializeTupleStruct, Self::Error> {
            self.0.serialize_tuple_struct(name, len).map(Wrap)
        }
        fn serialize_tuple_variant(
            self,
            name: &'static str,
            idx: u32,
            variant: &'static str,
            len: usize,
        ) -> Result<Self::SerializeTupleVariant, Self::Error> {
            self.0.serialize_tuple_variant(name, idx, variant, len).map(Wrap)
        }
        fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
            self.0.serialize_map(len).map(Wrap)
        }
        fn serialize_struct(self, name: &'static str, len: usize) -> Result<Self::SerializeStruct, Self::Error> {
            self.0.serialize_struct(name, len).map(Wrap)
        }
        fn serialize_struct_variant(
            self,
            name: &'static str,
            idx: u32,
            variant: &'static str,
            len: usize,
        ) -> Result<Self::SerializeStructVariant, Self::Error> {
            self.0.serialize_struct_variant(name, idx, variant, len).map(Wrap)
        }
    }

    impl<C: SerializeSeq> SerializeSeq for Wrap<C> {
        type Ok = C::Ok;
        type Error = C::Error;
        fn serialize_element<U: ?Sized + Serialize>(&mut self, value: &U) -> Result<(), Self::Error> {
            self.0.serialize_element(&Finite(value))
        }
        fn end(self) -> Result<Self::Ok, Self::Error> {
            self.0.end()
        }
    }
    impl<C: SerializeTuple> SerializeTuple for Wrap<C> {
        type Ok = C::Ok;
        type Error = C::Error;
        fn serialize_element<U: ?Sized + Serialize>(&mut self, value: &U) -> Result<(), Self::Error> {
            self.0.serialize_element(&Finite(value))
        }
        fn end(self) -> Result<Self::Ok, Self::Error> {
            self.0.end()
        }
    }
    impl<C: SerializeTupleStruct> SerializeTupleStruct for Wrap<C> {
        type Ok = C::Ok;
        type Error = C::Error;
        fn serialize_field<U: ?Sized + Serialize>(&mut self, value: &U) -> Result<(), Self::Error> {
            self.0.serialize_field(&Finite(value))
        }
        fn end(self) -> Result<Self::Ok, Self::Error> {
            self.0.end()
        }
    }
    impl<C: SerializeTupleVariant> SerializeTupleVariant for Wrap<C> {
        type Ok = C::Ok;
        type Error = C::Error;
        fn serialize_field<U: ?Sized + Serialize>(&mut self, value: &U) -> Result<(), Self::Error> {
            self.0.serialize_field(&Finite(value))
        }
        fn end(self) -> Result<Self::Ok, Self::Error> {
            self.0.end()
        }
    }
    impl<C: SerializeMap> SerializeMap for Wrap<C> {
        type Ok = C::Ok;
        type Error = C::Error;
        fn serialize_key<U: ?Sized + Serialize>(&mut self, key: &U) -> Result<(), Self::Error> {
            self.0.serialize_key(&Finite(key))
        }
        fn serialize_value<U: ?Sized + Serialize>(&mut self, value: &U) -> Result<(), Self::Error> {
            self.0.serialize_value(&Finite(value))
        }
        fn end(self) -> Result<Self::Ok, Self::Error> {
            self.0.end()
        }
    }
    impl<C: SerializeStruct> SerializeStruct for Wrap<C> {
        type Ok = C::Ok;
        type Error = C::Error;
        fn serialize_field<U: ?Sized + Serialize>(&mut self, key: &'static str, value: &U) -> Result<(), Self::Error> {
            self.0.serialize_field(key, &Finite(value))
        }
        fn end(self) -> Result<Self::Ok, Self::Error> {
            self.0.end()
        }
    }
    impl<C: SerializeStructVariant> SerializeStructVariant for Wrap<C> {
        type Ok = C::Ok;
        type Error = C::Error;
        fn serialize_field<U: ?Sized + Serialize>(&mut self, key: &'static str, value: &U) -> Result<(), Self::Error> {
            self.0.serialize_field(key, &Finite(value))
        }
        fn end(self) -> Result<Self::Ok, Self::Error> {
            self.0.end()
        }
    }

    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::pretty(&mut buf);
    Finite(value).serialize(&mut ser)?;
    Ok(buf)
}

/// The file name extension of a DesignCraft document.
pub const EXTENSION: &str = "designcraft";

/// Whether `bytes` are a DesignCraft document package: a zip holding `document.json`. Files
/// without an extension are recognised by this (the legacy JSON form is too generic to sniff).
pub fn is_designcraft(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK") && zip::ZipArchive::new(Cursor::new(bytes)).is_ok_and(|z| z.index_for_name("document.json").is_some())
}

/// Whether `prefix`, the start of a file, opens a DesignCraft package: a zip whose first, stored
/// entry is the `mimetype` naming this format. Packages saved before they had one are recognised
/// only whole ([`is_designcraft`]).
pub fn starts_designcraft(prefix: &[u8]) -> bool {
    prefix.starts_with(b"PK\x03\x04") && prefix.get(26..28) == Some(&8u16.to_le_bytes()[..]) && prefix.get(30..38) == Some(b"mimetype") && {
        let extra = prefix.get(28..30).map_or(0, |e| usize::from(u16::from_le_bytes([e[0], e[1]])));
        prefix.get(38 + extra..).is_some_and(|rest| rest.starts_with(MIME.as_bytes()))
    }
}

/// The zip entry holding the first page's picture.
pub const PREVIEW: &str = "preview.png";

/// The PNG bytes of a `.designcraft` file's preview, reading only the zip's directory and that
/// entry. None for files without one (saved before previews, or legacy JSON) or that aren't ours.
pub fn read_preview<R: Read + std::io::Seek>(file: R) -> Option<Vec<u8>> {
    let mut z = zip::ZipArchive::new(file).ok()?;
    let f = z.by_name(PREVIEW).ok()?;
    // A preview is small; a huge entry is not one we wrote.
    if f.size() > MAX_PREVIEW_BYTES {
        return None;
    }
    // The declared size can lie: bound what the entry inflates to as well.
    let mut v = Vec::new();
    f.take(MAX_PREVIEW_BYTES + 1).read_to_end(&mut v).ok()?;
    (u64::try_from(v.len()).ok()? <= MAX_PREVIEW_BYTES).then_some(v)
}

/// The largest preview read back (a first-page PNG is far smaller).
const MAX_PREVIEW_BYTES: u64 = 4 << 20;

/// Read `.designcraft` bytes (zip, or the legacy single JSON file).
pub fn load(bytes: &[u8]) -> Result<Document, FormatError> {
    if bytes.starts_with(b"PK") {
        return load_zip(bytes);
    }
    load_legacy_json(bytes)
}

/// Most entries a `.designcraft` archive may hold.
const MAX_ENTRIES: usize = 100_000;
/// Most bytes `meta.json` or `document.json` may expand to.
const MAX_JSON_BYTES: u64 = 256 << 20;
/// Most bytes one embedded asset may expand to.
const MAX_ASSET_BYTES: u64 = 1 << 30;
/// Most bytes all the entries read from one archive may expand to together.
const MAX_TOTAL_BYTES: u64 = 4 << 30;

/// Read entry `name` of `z`, or `None` when it is missing or unreadable. Refuses an entry that
/// expands past `limit` bytes or past what is `left` of the archive's budget, without
/// decompressing more than that: the sizes the archive claims are not trusted.
fn read_entry(z: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str, limit: u64, left: &mut u64) -> Result<Option<Vec<u8>>, FormatError> {
    let too_big = || FormatError::Io(format!("{name} is larger than the {limit}-byte limit (or the archive is larger than {MAX_TOTAL_BYTES} bytes)"));
    let Ok(f) = z.by_name(name) else { return Ok(None) };
    let limit = limit.min(*left);
    if f.size() > limit {
        return Err(too_big());
    }
    let mut v = Vec::new();
    if f.take(limit.saturating_add(1)).read_to_end(&mut v).is_err() {
        return Ok(None);
    }
    let n = v.len() as u64;
    if n > limit {
        return Err(too_big());
    }
    *left = left.saturating_sub(n);
    Ok(Some(v))
}

fn load_zip(bytes: &[u8]) -> Result<Document, FormatError> {
    let mut z = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| FormatError::NotOurs(e.to_string()))?;
    if z.len() > MAX_ENTRIES {
        return Err(FormatError::NotOurs(format!("{} entries (at most {MAX_ENTRIES})", z.len())));
    }
    let mut left = MAX_TOTAL_BYTES;
    let version = read_entry(&mut z, "meta.json", MAX_JSON_BYTES, &mut left)?
        .and_then(|meta| serde_json::from_slice::<Value>(&meta).ok())
        .and_then(|m| m.get("version").and_then(Value::as_u64))
        .unwrap_or(1);
    if version > u64::from(VERSION) {
        return Err(FormatError::TooNew(version));
    }
    let json = read_entry(&mut z, "document.json", MAX_JSON_BYTES, &mut left)?.ok_or_else(|| FormatError::NotOurs("missing document.json".into()))?;
    let mut v: Value = serde_json::from_slice(&json).map_err(|e| FormatError::NotOurs(e.to_string()))?;
    upgrade_json(&mut v);
    let mut doc: Document = serde_json::from_value(v).map_err(|e| FormatError::NotOurs(e.to_string()))?;
    let names: Vec<String> = z.file_names().map(str::to_string).collect();
    // Files saved with one copy per placement: identical copies share one buffer as they are
    // read, so memory holds each distinct file once.
    let fingerprint = std::collections::hash_map::RandomState::new();
    let mut loaded: HashMap<u64, Vec<Arc<Vec<u8>>>> = HashMap::new();
    for name in names {
        let Some(stem) = name.strip_prefix("assets/") else { continue };
        let Some(id) = stem.split('.').next().and_then(|s| s.parse::<u64>().ok()) else { continue };
        // Entries for assets the document doesn't have are never decompressed.
        if !doc.assets.contains_key(&AssetId(id)) {
            continue;
        }
        if let (Some(data), Some(a)) = (read_entry(&mut z, &name, MAX_ASSET_BYTES, &mut left)?, doc.assets.get_mut(&AssetId(id))) {
            let same = loaded.entry(fingerprint.hash_one(data.as_slice())).or_default();
            let data = match same.iter().find(|d| ***d == data) {
                Some(d) => d.clone(),
                None => {
                    let d = Arc::new(data);
                    same.push(d.clone());
                    d
                }
            };
            Arc::make_mut(a).data = data;
        }
    }
    upgrade(&mut doc, u32::try_from(version).unwrap_or(u32::MAX));
    doc.merge_duplicate_assets();
    doc.mark_differing_margins_local();
    doc.renumber_duplicate_items();
    doc.check().map_err(|e| FormatError::NotOurs(e.to_string()))?;
    Ok(doc)
}

/// Bring a document saved in format `from` up to [`VERSION`].
fn upgrade(doc: &mut Document, from: u32) {
    if from < 2 {
        // Format 1 laid out an unset Keep Lines Together mode as all lines in paragraph, and drop
        // caps without Align Left Edge.
        pin_para_default(doc, |a| &mut a.keep_all_lines, true);
        pin_para_default(doc, |a| &mut a.drop_cap_align_left, false);
    }
}

/// Pin a paragraph attribute to the value an older format resolved it to where nothing set it:
/// at the start of each paragraph style chain that leaves it unset, and on paragraphs whose style
/// is missing.
fn pin_para_default<T: Clone>(doc: &mut Document, field: fn(&mut ParaAttrs) -> &mut Option<T>, old: T) {
    let unset = |a: &ParaAttrs| field(&mut a.clone()).is_none();
    let roots: std::collections::HashSet<String> = doc
        .styles
        .paragraph
        .iter()
        .filter_map(|s| {
            let chain = doc.styles.para_chain(&s.name);
            if chain.iter().all(|c| unset(&c.para)) { chain.first().map(|c| c.name.clone()) } else { None }
        })
        .collect();
    if !roots.is_empty() {
        for s in doc.styles_mut().paragraph.iter_mut().filter(|s| roots.contains(&s.name)) {
            *field(&mut s.para) = Some(old.clone());
        }
    }
    let styles = doc.styles.clone();
    let mut pin = |st: &mut Story| {
        for p in st.paras.iter_mut().filter(|p| styles.para(&p.style).is_none()) {
            let v = field(&mut p.para);
            if v.is_none() {
                *v = Some(old.clone());
            }
        }
    };
    for st in doc.stories.values_mut() {
        let st = Arc::make_mut(st);
        st.for_each_text_mut(&mut pin);
        for n in &mut st.endnotes {
            Arc::make_mut(n).text.for_each_text_mut(&mut pin);
        }
    }
}

fn load_legacy_json(bytes: &[u8]) -> Result<Document, FormatError> {
    let mut v: Value = serde_json::from_slice(bytes).map_err(|e| FormatError::NotOurs(e.to_string()))?;
    upgrade_json(&mut v);
    let mut doc: Document = serde_json::from_value(v.clone()).map_err(|e| FormatError::NotOurs(e.to_string()))?;
    if let Some(data) = v.get("assetData").and_then(Value::as_object) {
        for (k, s) in data {
            if let (Ok(id), Some(s)) = (k.parse::<u64>(), s.as_str())
                && let Some(a) = doc.assets.get_mut(&AssetId(id))
            {
                Arc::make_mut(a).data = Arc::new(base64_decode(s));
            }
        }
    }
    upgrade(&mut doc, 1);
    doc.merge_duplicate_assets();
    doc.mark_differing_margins_local();
    doc.renumber_duplicate_items();
    doc.check().map_err(|e| FormatError::NotOurs(e.to_string()))?;
    Ok(doc)
}

/// Bring older documents' JSON up to the current model. Vertical Type was a text frame option
/// (`content.options.vertical`); it is the story's direction, which a story takes from its first
/// frame.
fn upgrade_json(doc: &mut Value) {
    let mut vertical = HashSet::new();
    vertical_frames(doc, &mut vertical, 0);
    if vertical.is_empty() {
        return;
    }
    let Some(stories) = doc.get_mut("stories").and_then(Value::as_object_mut) else { return };
    for st in stories.values_mut() {
        let first = st.get("frames").and_then(|f| f.get(0)).and_then(Value::as_u64);
        if let (Some(f), Some(st)) = (first, st.as_object_mut())
            && vertical.contains(&f)
            && !st.contains_key("vertical")
        {
            st.insert("vertical".into(), Value::Bool(true));
        }
    }
}

/// Ids of the text frames in `v` (any depth: groups, anchored objects) whose options say vertical.
fn vertical_frames(v: &Value, out: &mut HashSet<u64>, depth: usize) {
    // serde_json already refuses JSON nested deeper than 128 levels.
    if depth > 256 {
        return;
    }
    match v {
        Value::Object(m) => {
            if let (Some(id), Some(c)) = (m.get("id").and_then(Value::as_u64), m.get("content"))
                && c.get("type").and_then(Value::as_str) == Some("text")
                && c.get("options").and_then(|o| o.get("vertical")).and_then(Value::as_bool) == Some(true)
            {
                out.insert(id);
            }
            for c in m.values() {
                vertical_frames(c, out, depth + 1);
            }
        }
        Value::Array(a) => {
            for c in a {
                vertical_frames(c, out, depth + 1);
            }
        }
        _ => {}
    }
}

fn base64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => continue,
        } as u32;
        buf = buf << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests;
