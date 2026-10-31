//! Header-before-decode inline PNG/JPEG validation and resource bounds for `file.place`.

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::bad;
use super::{ID, MAX_ENCODED_IMAGE_BYTES, MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS};
use crate::Result;

pub(super) struct CheckedRaster {
    pub(super) sha256: [u8; 32],
    pub(super) bytes: usize,
    pub(super) pixels: u64,
}

pub(super) fn check_image(p: &Value) -> Result<CheckedRaster> {
    if p.get("frame").is_none() && !["x", "y", "width"].iter().all(|key| p.get(key).is_some()) {
        return Err(bad(ID, "image placement needs an explicit frame or x, y and width; loading the place cursor is not permitted"));
    }
    let encoded = p.get("base64").and_then(Value::as_str).ok_or_else(|| bad(ID, "inline image required"))?;
    if encoded.len() > MAX_ENCODED_IMAGE_BYTES {
        return Err(bad(ID, "inline image exceeds the 16 MiB encoded raster allowance"));
    }
    let name = p.get("name").and_then(Value::as_str).ok_or_else(|| bad(ID, "image name required"))?.to_ascii_lowercase();
    let bytes = super::super::base64_decode(encoded);
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
