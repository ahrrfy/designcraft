//! IDML (InDesign Markup Language) interchange: [`export_idml`] and [`import_idml`].
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
//!
//! Implemented clean-room from the public IDML File Format Specification and from inspecting
//! IDML packages exported from synthetic documents. An IDML package is a ZIP whose first entry is
//! an uncompressed `mimetype`, followed by `designmap.xml` (the root `Document` element that
//! includes the other parts) and the `Resources/`, `MasterSpreads/`, `Spreads/` and `Stories/`
//! parts.
//!
//! ## Coordinates
//! DesignCraft spreads have their origin at the top-left of the first page (y down). IDML spread
//! coordinates have their origin at the binding spine (facing pages) or the spread centre
//! (single-sided documents), vertically centred on the pages; pages are placed by their
//! `ItemTransform` and every page item's geometry is `PathGeometry` in inner coordinates mapped by
//! `ItemTransform` (inner → parent). Export maps `xf_idml = T(-origin) · xf`; import computes the
//! page rectangles from the page transforms and maps back.
//!
//! ## Supported subset
//! Swatches (process/spot CMYK, RGB and Lab colours, tints, linear/radial gradients), paragraph,
//! character and object styles (with `BasedOn` and style groups), fonts, document preferences
//! (page size, facing pages, bleed/slug, margins and columns including dragged column positions,
//! units, grids), ruler guides, layers, sections,
//! parent (master) spreads and applied parents, spreads and pages, page items (`TextFrame`,
//! `Rectangle`, `Oval`, `Polygon`, `GraphicLine`, `Group`) with fill/stroke, corner options, text
//! frame options (with column rules), text wrap, transparency (opacity, blend mode, drop shadow,
//! feather, gradient feather, directional feather), images (embedded `Contents` or linked), stories with paragraph/character style ranges and local overrides, special
//! characters and breaks, text threading, ruler and liquid guides, and stored page liquid rules.
//!
//! ## TODO (ignored on import / not written on export)
//! - Tables, footnotes, anchored/inline objects, notes, tracked changes, hyperlinks, index and
//!   cross-reference markers, text variables, conditions, XML structure (`XML/*` parts).
//! - Page-item overrides of parent items (only the page's `OverrideList` is read),
//!   parent-of-parent page mapping beyond `AppliedMaster`, alternate layouts, liquid object constraints.
//! - Nested/GREP/line styles, bullets & numbering details (bullet glyph, number format), OpenType
//!   feature flags, paragraph borders, grid alignment, custom dashed stroke styles; column rule
//!   stroke type and overprint.
//! - Mixed inks, gradient stop opacity, gradient feather stops between the first and last, its
//!   midpoints and highlight, directional feather angle, noise, choke and shape mode, other effects
//!   than drop shadow and the feathers, stroke/fill/content transparency, EPS/PDF/AI placed graphics (imported as images when the data is available),
//!   clipping paths, compound-path fill rules.
//! - Placed PDFs: the page (`PageNumber`) and crop choice (`PDFCrop`) round-trip, and
//!   `GraphicBounds` span the chosen box. Where that box sits on the page needs the PDF parsed,
//!   which this crate doesn't do: the engine finds it after import (`file.openIdml`), and when the
//!   PDF's data isn't available the page's crop box fills `GraphicBounds`. The content crops
//!   (`CropContentVisibleLayers`, `CropContentAllLayers`) are the bounds of what the page draws:
//!   its paths, text and images, each cut to the clip in force.
//! - Unknown elements are ignored; nothing is preserved opaquely for round-trip yet.

mod arabic;
mod cjk;
mod export;
mod import;
mod names;
mod ruby;
mod xml;

#[cfg(test)]
mod tests;

pub use export::{ExportOptions, export_idml, export_idml_with};
pub use import::{Imported, import_idml, import_idml_report, import_idml_report_with_previews, import_idml_with};

/// The IDML package mimetype (content of the first, stored `mimetype` entry).
pub const MIMETYPE: &str = "application/vnd.adobe.indesign-idml-package";
/// The DOM version written to every part.
pub const DOM_VERSION: &str = "16.0";

#[derive(Debug, thiserror::Error)]
pub enum IdmlError {
    #[error("not an IDML package: {0}")]
    NotIdml(String),
    #[error("IDML part {part}: {msg}")]
    Part { part: String, msg: String },
    #[error("invalid IDML document: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, IdmlError>;

/// Does `bytes` look like an IDML package (a ZIP whose first, stored entry is the IDML `mimetype`)?
pub fn is_idml(bytes: &[u8]) -> bool {
    bytes.starts_with(b"PK\x03\x04") && bytes.get(30..38) == Some(b"mimetype") && {
        let extra = bytes.get(28..30).map_or(0, |e| u16::from_le_bytes([e[0], e[1]]) as usize);
        bytes.get(38 + extra..).is_some_and(|rest| rest.starts_with(MIMETYPE.as_bytes()))
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64 with line breaks every 76 characters (as embedded image `Contents` are written).
pub(crate) fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4 + data.len() / 57);
    for (i, c) in data.chunks(3).enumerate() {
        if i > 0 && i % 19 == 0 {
            out.push('\n');
        }
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Lenient base64 decoding (whitespace and unknown characters are skipped).
pub(crate) fn base64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
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

/// Sniff an image's MIME type and pixel size (PNG, JPEG, GIF, WebP, TIFF, PDF, EPS).
pub(crate) fn sniff_image(b: &[u8]) -> (Option<&'static str>, Option<(u32, u32)>) {
    if designcraft_images::is_eps(b) {
        return (Some("application/postscript"), designcraft_images::eps_pixel_size(b));
    }
    let be32 = |i: usize| b.get(i..i + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]));
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return (Some("image/png"), be32(16).zip(be32(20)));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            while b.get(i) == Some(&0xFF) {
                i += 1;
            }
            let Some(&m) = b.get(i) else { break };
            i += 1;
            if m == 0x00 || matches!(m, 0x01 | 0xD0..=0xD8) {
                continue;
            }
            if m == 0xD9 || m == 0xDA {
                break;
            }
            let Some(len_bytes) = b.get(i..i + 2) else { break };
            let len = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
            if len < 2 {
                break;
            }
            if matches!(m, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
                if len >= 7
                    && let Some(sof) = b.get(i + 3..i + 7)
                {
                    let h = u32::from(u16::from_be_bytes([sof[0], sof[1]]));
                    let w = u32::from(u16::from_be_bytes([sof[2], sof[3]]));
                    return (Some("image/jpeg"), Some((w, h)));
                }
                break;
            }
            let Some(next) = i.checked_add(len) else { break };
            i = next;
        }
        return (Some("image/jpeg"), None);
    }
    if b.starts_with(b"GIF8") && b.len() >= 10 {
        return (Some("image/gif"), Some((u16::from_le_bytes([b[6], b[7]]) as u32, u16::from_le_bytes([b[8], b[9]]) as u32)));
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return (Some("image/webp"), None);
    }
    if b.starts_with(b"II*\0") || b.starts_with(b"MM\0*") {
        return (Some("image/tiff"), None);
    }
    if b.starts_with(b"%PDF") {
        return (Some("application/pdf"), None);
    }
    (None, None)
}
