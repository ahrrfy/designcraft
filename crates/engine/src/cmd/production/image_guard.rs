//! Header-before-decode inline PNG/JPEG validation and resource bounds for `file.place`.

use designcraft_doc::{AssetId, Document};
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

fn validate_raster_bytes(bytes: &[u8], name: Option<&str>) -> Result<CheckedRaster> {
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(bad(ID, "inline image exceeds 16 MiB of raster bytes"));
    }
    let png = bytes.starts_with(b"\x89PNG\r\n\x1a\n") && name.is_none_or(|n| n.ends_with(".png"));
    let jpeg = bytes.starts_with(&[0xff, 0xd8, 0xff]) && name.is_none_or(|n| n.ends_with(".jpg") || n.ends_with(".jpeg"));
    if !png && !jpeg {
        return Err(bad(ID, "only inline PNG/JPEG image bytes are permitted; SVG, PDF, paths and media are excluded"));
    }
    // Header inspection does not decode pixel data. Refuse PNG/JPEG decompression bombs first.
    let (w, h) = designcraft_images::pixel_size(bytes).ok_or_else(|| bad(ID, "invalid raster image header"))?;
    let pixels = u64::from(w) * u64::from(h);
    if w == 0 || h == 0 || pixels > MAX_IMAGE_PIXELS {
        return Err(bad(ID, "image dimensions exceed 20 million pixels"));
    }
    Ok(CheckedRaster { sha256: Sha256::digest(bytes).into(), bytes: bytes.len(), pixels })
}

pub(super) fn check_image(doc: &Document, p: &Value) -> Result<CheckedRaster> {
    if p.get("frame").is_none() && !["x", "y", "width"].iter().all(|key| p.get(key).is_some()) {
        return Err(bad(ID, "image placement needs an explicit frame or x, y and width; loading the place cursor is not permitted"));
    }
    let has_base64 = p.get("base64").is_some();
    let has_reuse = p.get("reuseAsset").is_some();
    if has_base64 && has_reuse {
        return Err(bad(ID, "specify either `base64` or `reuseAsset`, not both"));
    }
    if let Some(reuse_val) = p.get("reuseAsset") {
        let aid = reuse_val.as_u64().filter(|&n| n >= 1).ok_or_else(|| bad(ID, "`reuseAsset` must be a positive asset id"))?;
        let asset = doc.assets.get(&AssetId(aid)).ok_or_else(|| bad(ID, format!("reuseAsset: no such asset {aid}")))?;
        if asset.link.is_some() || asset.data.is_empty() {
            return Err(bad(ID, "reuseAsset requires an embedded raster asset"));
        }
        let name_override = p.get("name").and_then(Value::as_str).map(str::to_ascii_lowercase);
        return validate_raster_bytes(&asset.data, name_override.as_deref());
    }
    let encoded = p.get("base64").and_then(Value::as_str).ok_or_else(|| bad(ID, "inline image or reuseAsset required"))?;
    if encoded.len() > MAX_ENCODED_IMAGE_BYTES {
        return Err(bad(ID, "inline image exceeds the 16 MiB encoded raster allowance"));
    }
    let name = p.get("name").and_then(Value::as_str).ok_or_else(|| bad(ID, "image name required"))?.to_ascii_lowercase();
    let bytes = super::super::base64_decode(encoded);
    validate_raster_bytes(&bytes, Some(&name))
}
