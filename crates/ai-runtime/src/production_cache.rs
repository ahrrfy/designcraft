//! Bounded local reuse of validated production results, never execution authority.
//! Callers must revalidate every hit against current native, copy and geometry rules.
use base64::Engine;
use designcraft_ai_providers::GeneratedImage;
use designcraft_ai_types::protocol::ProductionPlan;
use designcraft_engine::{file_access, io};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

const VERSION: u32 = 1;
const MAX_ENTRY_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ENTRIES: usize = 512;
const TARGET_ENTRIES: usize = (MAX_ENTRIES * 85) / 100;
const TARGET_CACHE_BYTES: u64 = (MAX_CACHE_BYTES * 85) / 100;
const REJECTED_RESPONSE_FILE: &str = "last-rejected-response.txt";
const MAX_SCAN_FILES: usize = 8192;
const MAX_NODES: usize = 65_536;
static WRITES: Mutex<()> = Mutex::new(());

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    version: u32,
    kind: String,
    key: String,
    payload_sha256: String,
    payload: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImagePayload {
    filename: String,
    mime_type: String,
    base64: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BinaryImagePayload {
    filename: String,
    mime_type: String,
    blob_sha256: String,
    bytes: usize,
}

/// Hash the complete caller-supplied cache identity without persisting its inputs.
pub(crate) fn digest(value: &Value) -> Result<String, String> {
    check_value(value)?;
    Ok(hash(&encode(value)?))
}

pub(crate) fn load_plan(root: &Path, key: &str) -> Result<Option<ProductionPlan>, String> {
    let Some(payload) = load(root, key, "plan")? else { return Ok(None) };
    let plan: ProductionPlan = serde_json::from_value(payload).map_err(|e| format!("Production cache plan is corrupt: {e}"))?;
    validate_plan(&plan)?;
    Ok(Some(plan))
}

pub(crate) fn store_plan(root: &Path, key: &str, plan: &ProductionPlan) -> Result<(), String> {
    validate_plan(plan)?;
    let payload = serde_json::from_slice(&encode(plan)?).map_err(|e| format!("Cannot encode production cache plan: {e}"))?;
    store(root, key, "plan", payload)
}

/// One bounded private diagnostic, never an executable or validated plan entry.
/// Replacement uses the same ordinary-file, atomic-write and shared quota guards.
pub(crate) fn store_rejected_response(root: &Path, response: &str) -> Result<Option<PathBuf>, String> {
    if response.len() > 4 * 1024 * 1024 {
        return Err("Rejected response exceeds its 4-MiB diagnostic limit".into());
    }
    let path = root.join(REJECTED_RESPONSE_FILE);
    Ok(write_cache_file(root, &path, response.as_bytes())?.then_some(path))
}

pub(crate) fn load_image(root: &Path, key: &str) -> Result<Option<GeneratedImage>, String> {
    let Some(payload) = load(root, key, "image")? else { return Ok(None) };
    if payload.get("blob_sha256").is_some() {
        let metadata: BinaryImagePayload = serde_json::from_value(payload).map_err(|e| format!("Production cache image metadata is corrupt: {e}"))?;
        let sha256 = normalized_key(&metadata.blob_sha256)?;
        if metadata.bytes == 0 || metadata.bytes > MAX_IMAGE_BYTES {
            return Err("Production cache image exceeds its binary byte limit".into());
        }
        let path = root.join(format!("raster-{sha256}.bin"));
        file_access::check_read(&path)?;
        let stat = std::fs::symlink_metadata(&path).map_err(|e| format!("Cannot inspect production image blob: {e}"))?;
        if is_link(&stat) || !stat.is_file() || stat.len() > MAX_IMAGE_BYTES as u64 {
            return Err("Production cache image blob is not a bounded ordinary file".into());
        }
        let bytes = io::read_capped(&path, MAX_IMAGE_BYTES as u64).map_err(|e| format!("Cannot read production image blob: {e}"))?;
        if bytes.len() != metadata.bytes || hash(&bytes) != sha256 {
            return Err("Production cache image blob length or checksum is invalid".into());
        }
        let image = GeneratedImage { bytes, filename: metadata.filename, mime_type: metadata.mime_type };
        validate_image(&image)?;
        return Ok(Some(image));
    }
    // Existing small inline entries remain readable after upgrading the application.
    let image: ImagePayload = serde_json::from_value(payload).map_err(|e| format!("Production cache image is corrupt: {e}"))?;
    let encoded_cap = MAX_IMAGE_BYTES.saturating_add(2) / 3 * 4;
    if image.base64.len() > encoded_cap {
        return Err("Production cache image exceeds its encoded byte limit".into());
    }
    let image = GeneratedImage {
        bytes: base64::engine::general_purpose::STANDARD
            .decode(image.base64)
            .map_err(|e| format!("Production cache image base64 is corrupt: {e}"))?,
        filename: image.filename,
        mime_type: image.mime_type,
    };
    validate_image(&image)?;
    Ok(Some(image))
}

pub(crate) fn store_image(root: &Path, key: &str, image: &GeneratedImage) -> Result<(), String> {
    let key = normalized_key(key)?;
    validate_image(image)?;
    let sha256 = hash(&image.bytes);
    let blob_path = root.join(format!("raster-{sha256}.bin"));
    if !write_cache_file(root, &blob_path, &image.bytes)? {
        return Ok(());
    }
    let payload = serde_json::to_value(BinaryImagePayload {
        filename: image.filename.clone(),
        mime_type: image.mime_type.clone(),
        blob_sha256: sha256,
        bytes: image.bytes.len(),
    })
    .map_err(|e| format!("Cannot encode production cache image: {e}"))?;
    let entry = Entry { version: VERSION, kind: "image".into(), key: key.clone(), payload_sha256: digest(&payload)?, payload };
    let bytes = encode(&entry)?;
    let meta_path = entry_path(root, &key, "image");
    write_cache_file_protecting(root, &meta_path, &bytes, Some(&blob_path)).map(|_| ())
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn normalized_key(key: &str) -> Result<String, String> {
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Production cache key must be exactly 64 hexadecimal SHA-256 characters".into());
    }
    Ok(key.to_ascii_lowercase())
}

fn checked_root(root: &Path) -> Result<(), String> {
    if !root.is_absolute() || root.components().any(|part| matches!(part, Component::ParentDir | Component::CurDir)) {
        return Err("Production cache requires an absolute directory without traversal components".into());
    }
    file_access::check_read(root)?;
    // Check every ancestor before reading directory contents, including Windows
    // junctions. Like engine file access, this does not defeat hostile OS races.
    for ancestor in root.ancestors() {
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) if is_link(&metadata) => return Err("Production cache paths must not contain symbolic links or reparse points".into()),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("Cannot inspect production cache directory: {error}")),
        }
    }
    Ok(())
}

fn is_link(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn entry_path(root: &Path, key: &str, kind: &str) -> PathBuf {
    root.join(format!("{key}.{kind}.json"))
}

fn load(root: &Path, key: &str, kind: &str) -> Result<Option<Value>, String> {
    let key = normalized_key(key)?;
    checked_root(root)?;
    let path = entry_path(root, &key, kind);
    file_access::check_read(&path)?;
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            if is_link(&metadata) || !metadata.is_file() || metadata.len() > MAX_ENTRY_BYTES as u64 {
                return Err("Production cache entry is not a bounded ordinary file".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot inspect production cache entry: {error}")),
    }
    let bytes = io::read_capped(&path, MAX_ENTRY_BYTES as u64).map_err(|e| format!("Cannot read production cache entry: {e}"))?;
    if bytes.len() > MAX_ENTRY_BYTES {
        return Err("Production cache entry grew beyond its byte limit".into());
    }
    let entry: Entry = serde_json::from_slice(&bytes).map_err(|e| format!("Production cache entry is corrupt: {e}"))?;
    if entry.version != VERSION || entry.kind != kind || entry.key != key || entry.payload_sha256 != digest(&entry.payload)? {
        return Err("Production cache entry version, identity or content checksum is invalid".into());
    }
    Ok(Some(entry.payload))
}

/// Oldest cache entries are evicted down to 85% of entry/byte caps when full.
fn store(root: &Path, key: &str, kind: &str, payload: Value) -> Result<(), String> {
    let key = normalized_key(key)?;
    let entry = Entry { version: VERSION, kind: kind.into(), key: key.clone(), payload_sha256: digest(&payload)?, payload };
    let bytes = encode(&entry)?;
    let path = entry_path(root, &key, kind);
    write_cache_file(root, &path, &bytes).map(|_| ())
}

struct DiskCacheItem {
    path: PathBuf,
    file_name: String,
    len: u64,
    modified: std::time::SystemTime,
}

fn read_image_blob_ref(path: &Path) -> Option<String> {
    let bytes = io::read_capped(path, 65_536).ok()?;
    let entry: Entry = serde_json::from_slice(&bytes).ok()?;
    entry.payload.get("blob_sha256").and_then(Value::as_str).map(str::to_ascii_lowercase)
}

fn write_cache_file(root: &Path, path: &Path, bytes: &[u8]) -> Result<bool, String> {
    write_cache_file_protecting(root, path, bytes, None)
}

// Binary blobs and metadata share the same entry and byte quotas, including atomic-write
// staging space. When full, oldest `.json`/`.bin` entries are evicted down to 85% of cap;
// `last-rejected-response.txt` is never evicted and can always overwrite itself.
fn write_cache_file_protecting(root: &Path, path: &Path, bytes: &[u8], protected: Option<&Path>) -> Result<bool, String> {
    let _lock = WRITES.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    checked_root(root)?;
    file_access::check_write(root)?;
    std::fs::create_dir_all(root).map_err(|e| format!("Cannot create production cache directory: {e}"))?;
    checked_root(root)?;
    file_access::check_write(path)?;
    let incoming_len = bytes.len() as u64;
    if incoming_len > MAX_CACHE_BYTES {
        return Ok(false);
    }

    let is_diagnostic = path.file_name().and_then(|n| n.to_str()) == Some(REJECTED_RESPONSE_FILE);
    let mut scanned = 0usize;
    let mut cache_entries = 0usize;
    let mut total = 0u64;
    let mut replaced = None;
    let mut items = Vec::new();

    for item in std::fs::read_dir(root).map_err(|e| format!("Cannot inspect production cache quota: {e}"))? {
        scanned = scanned.checked_add(1).ok_or("Production cache entry count overflow")?;
        if scanned > MAX_SCAN_FILES {
            return Err("Production cache directory exceeds maximum scan safety limit".into());
        }
        let item = item.map_err(|e| format!("Cannot inspect production cache entry: {e}"))?;
        let candidate = item.path();
        file_access::check_read(&candidate)?;
        let metadata = std::fs::symlink_metadata(&candidate).map_err(|e| format!("Cannot inspect production cache entry: {e}"))?;
        if is_link(&metadata) || !metadata.is_file() {
            return Err("Production cache directory must contain only ordinary files".into());
        }
        let len = metadata.len();
        total = total.checked_add(len).ok_or("Production cache byte count overflow")?;
        if candidate == path {
            replaced = Some(len);
        }
        let file_name = candidate.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if file_name != REJECTED_RESPONSE_FILE {
            cache_entries = cache_entries.checked_add(1).ok_or("Production cache entry count overflow")?;
            let is_evictable_ext = matches!(candidate.extension().and_then(|e| e.to_str()), Some("json" | "bin"));
            if is_evictable_ext && candidate != path && protected.is_none_or(|p| p != candidate) {
                let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
                items.push(DiskCacheItem { path: candidate, file_name, len, modified });
            }
        }
    }

    let projected_entries = |entries: usize| -> usize { if is_diagnostic || replaced.is_some() { entries } else { entries.saturating_add(1) } };
    let exceeds_max = |entries: usize, bytes_total: u64| -> Result<bool, String> {
        let proj =
            bytes_total.checked_sub(replaced.unwrap_or(0)).and_then(|n| n.checked_add(incoming_len)).ok_or("Production cache byte count overflow")?;
        let staged = bytes_total.checked_add(incoming_len).ok_or("Production cache staging byte count overflow")?;
        Ok((!is_diagnostic && projected_entries(entries) > MAX_ENTRIES) || proj > MAX_CACHE_BYTES || staged > MAX_CACHE_BYTES)
    };

    if exceeds_max(cache_entries, total)? {
        items.sort_by(|a, b| a.modified.cmp(&b.modified).then_with(|| a.file_name.cmp(&b.file_name)));
        let mut image_refs: std::collections::BTreeMap<PathBuf, String> = std::collections::BTreeMap::new();
        for item in &items {
            if item.file_name.ends_with(".image.json")
                && let Some(sha) = read_image_blob_ref(&item.path)
            {
                image_refs.insert(item.path.clone(), sha);
            }
        }
        let by_path: std::collections::BTreeMap<PathBuf, u64> = items.iter().map(|i| (i.path.clone(), i.len)).collect();
        let mut removed: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();

        let target_bytes_cap = TARGET_CACHE_BYTES.max(incoming_len);
        let needs_more_eviction = |entries: usize, bytes_total: u64| -> bool {
            let proj = bytes_total.saturating_sub(replaced.unwrap_or(0)).saturating_add(incoming_len);
            let staged = bytes_total.saturating_add(incoming_len);
            (!is_diagnostic && projected_entries(entries) > TARGET_ENTRIES) || proj > target_bytes_cap || staged > target_bytes_cap
        };

        for item in &items {
            if !needs_more_eviction(cache_entries, total) {
                break;
            }
            if removed.contains(&item.path) {
                continue;
            }
            let mut batch_to_remove = Vec::new();
            if let Some(sha) = item.file_name.strip_prefix("raster-").and_then(|s| s.strip_suffix(".bin")) {
                let sha_lower = sha.to_ascii_lowercase();
                for (img_path, ref_sha) in &image_refs {
                    if *ref_sha == sha_lower && !removed.contains(img_path) {
                        batch_to_remove.push(img_path.clone());
                    }
                }
                batch_to_remove.push(item.path.clone());
            } else if let Some(ref_sha) = image_refs.get(&item.path).cloned() {
                batch_to_remove.push(item.path.clone());
                let still_referenced = image_refs
                    .iter()
                    .any(|(other_path, other_sha)| other_path != &item.path && !removed.contains(other_path) && *other_sha == ref_sha);
                let blob_candidate = root.join(format!("raster-{ref_sha}.bin"));
                if !still_referenced
                    && by_path.contains_key(&blob_candidate)
                    && !removed.contains(&blob_candidate)
                    && protected.is_none_or(|p| p != blob_candidate)
                    && blob_candidate != path
                {
                    batch_to_remove.push(blob_candidate);
                }
            } else {
                batch_to_remove.push(item.path.clone());
            }

            for victim in batch_to_remove {
                if removed.contains(&victim) {
                    continue;
                }
                file_access::check_write(&victim)?;
                if std::fs::remove_file(&victim).is_ok() {
                    if let Some(victim_len) = by_path.get(&victim).copied() {
                        total = total.saturating_sub(victim_len);
                        cache_entries = cache_entries.saturating_sub(1);
                    }
                    removed.insert(victim);
                }
            }
        }

        if exceeds_max(cache_entries, total)? {
            return Ok(false);
        }
    }

    // Recheck the target after scanning/eviction, before the engine's atomic replacement.
    if let Ok(metadata) = std::fs::symlink_metadata(path)
        && (is_link(&metadata) || !metadata.is_file())
    {
        return Err("Production cache target is not an ordinary file".into());
    }
    io::write_file(path, bytes).map_err(|e| format!("Cannot write production cache entry: {e}"))?;
    Ok(true)
}

fn validate_plan(plan: &ProductionPlan) -> Result<(), String> {
    if plan.title.trim().is_empty() || plan.title.len() > 256 || !(1..=512).contains(&plan.commands.len()) || plan.images.len() > 8 {
        return Err("Production cache plan title or action/image count is invalid".into());
    }
    for action in &plan.commands {
        if action.command.is_empty() || action.command.len() > 128 || !action.params.is_object() {
            return Err("Production cache plan contains an invalid action".into());
        }
        check_value(&action.params)?;
    }
    for image in &plan.images {
        if image.prompt.trim().is_empty()
            || image.prompt.len() > 8192
            || image.label.trim().is_empty()
            || image.label.len() > 256
            || image.family_id.as_deref().is_some_and(|s| s.trim().is_empty() || s.len() > 64)
            || image.slot_id.as_deref().is_some_and(|s| s.trim().is_empty() || s.len() > 64)
            || image.page > 999
            || image.rect.iter().any(|n| !n.is_finite() || n.abs() > 15552.0)
            || image.rect[2] <= image.rect[0]
            || image.rect[3] <= image.rect[1]
        {
            return Err("Production cache plan contains an invalid illustration placement".into());
        }
    }
    Ok(())
}

fn validate_image(image: &GeneratedImage) -> Result<(), String> {
    let valid_header = match image.mime_type.as_str() {
        "image/png" => image.bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => image.bytes.starts_with(&[0xff, 0xd8, 0xff]),
        _ => false,
    };
    if !valid_header
        || image.bytes.len() > MAX_IMAGE_BYTES
        || image.filename.is_empty()
        || image.filename.len() > 160
        || matches!(image.filename.as_str(), "." | "..")
        || image.filename.contains(['/', '\\', ':', '\0'])
    {
        return Err("Production cache image type, filename or byte limit is invalid".into());
    }
    let (width, height) = designcraft_images::pixel_size(&image.bytes).ok_or("Production cache image header is corrupt")?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 20_000_000 {
        return Err("Production cache image pixel limit is invalid".into());
    }
    Ok(())
}

fn check_value(value: &Value) -> Result<(), String> {
    let mut pending = vec![(value, 0usize)];
    let mut visited = 0usize;
    let mut text_bytes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        visited = visited.checked_add(1).ok_or("Production cache JSON node count overflow")?;
        if depth > 64 || visited > MAX_NODES {
            return Err("Production cache JSON exceeds its depth or node limit".into());
        }
        let children = match value {
            Value::Array(values) => values.len(),
            Value::Object(values) => values.len(),
            _ => 0,
        };
        if pending.len().checked_add(children).is_none_or(|n| n > MAX_NODES) {
            return Err("Production cache JSON exceeds its pending node limit".into());
        }
        let length = match value {
            Value::String(text) => text.len(),
            Value::Object(values) => {
                values.keys().try_fold(0usize, |size, key| size.checked_add(key.len())).ok_or("Production cache key byte count overflow")?
            }
            _ => 0,
        };
        text_bytes =
            text_bytes.checked_add(length).filter(|length| *length <= MAX_ENTRY_BYTES).ok_or("Production cache JSON text exceeds its byte limit")?;
        match value {
            Value::Array(values) => pending.extend(values.iter().map(|value| (value, depth + 1))),
            Value::Object(values) => pending.extend(values.values().map(|value| (value, depth + 1))),
            _ => {}
        }
    }
    Ok(())
}

struct CappedWriter(Vec<u8>);
impl std::io::Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().checked_add(bytes.len()).is_none_or(|n| n > MAX_ENTRY_BYTES) {
            return Err(std::io::Error::other("Production cache JSON exceeds 8 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode(value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut writer = CappedWriter(Vec::new());
    serde_json::to_writer(&mut writer, value).map_err(|e| format!("Cannot encode bounded production cache entry: {e}"))?;
    Ok(writer.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use designcraft_ai_types::EngineAction;
    use serde_json::json;

    fn temporary_root() -> tempfile::TempDir {
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
    }

    fn plan() -> ProductionPlan {
        ProductionPlan {
            title: "Validated draft".into(),
            commands: vec![EngineAction { command: "frame.create".into(), params: json!({"rect":[60,60,160,160]}) }],
            images: vec![],
        }
    }
    fn key() -> String {
        digest(&json!({"model":"mock","prompt":"Exact brief","schema":{},"scope":[0,2],"revision":"pinned"})).unwrap()
    }

    #[test]
    fn print_rasters_above_four_mib_roundtrip_as_bounded_binary_blobs() {
        let root = temporary_root();
        let mut encoded = std::io::Cursor::new(Vec::new());
        let mut seed = 17u32;
        let pixels = image::RgbImage::from_fn(1400, 1100, |_, _| {
            let mut rgb = [0; 3];
            for channel in &mut rgb {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                *channel = seed as u8;
            }
            image::Rgb(rgb)
        });
        pixels.write_to(&mut encoded, image::ImageFormat::Png).unwrap();
        let image = GeneratedImage { bytes: encoded.into_inner(), filename: "print.png".into(), mime_type: "image/png".into() };
        assert!(image.bytes.len() > 4 * 1024 * 1024);
        store_image(root.path(), &key(), &image).unwrap();
        let cached = load_image(root.path(), &key()).unwrap().unwrap();
        assert_eq!(cached.bytes, image.bytes);
        let metadata = std::fs::read(entry_path(root.path(), &key(), "image")).unwrap();
        assert!(metadata.len() < 2048, "Image bytes must not inflate JSON metadata");
        let binary: Vec<_> =
            std::fs::read_dir(root.path()).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|e| e == "bin")).collect();
        assert_eq!(binary.len(), 1);
        assert_eq!(std::fs::read(&binary[0]).unwrap(), image.bytes);
        std::fs::write(&binary[0], b"replaced").unwrap();
        assert!(load_image(root.path(), &key()).unwrap_err().contains("checksum"));
    }

    #[test]
    fn square_and_non_square_four_k_cache_headers_fit_the_pixel_policy() {
        for (width, height) in [(4096, 4096), (5056, 3392)] {
            let mut png = std::io::Cursor::new(Vec::new());
            image::RgbImage::new(width, height).write_to(&mut png, image::ImageFormat::Png).unwrap();
            validate_image(&GeneratedImage { bytes: png.into_inner(), filename: "4k.png".into(), mime_type: "image/png".into() }).unwrap();
        }
    }

    #[test]
    fn legacy_inline_images_remain_readable_and_blob_traversal_is_rejected() {
        let root = temporary_root();
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbImage::new(2, 2).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let bytes = png.into_inner();
        store(
            root.path(),
            &key(),
            "image",
            json!({"filename":"legacy.png","mime_type":"image/png","base64":base64::engine::general_purpose::STANDARD.encode(&bytes)}),
        )
        .unwrap();
        assert_eq!(load_image(root.path(), &key()).unwrap().unwrap().bytes, bytes);
        store(root.path(), &key(), "image", json!({"filename":"legacy.png","mime_type":"image/png","blob_sha256":"../outside","bytes":12})).unwrap();
        assert!(load_image(root.path(), &key()).is_err());
    }

    #[test]
    fn validated_results_roundtrip_and_complete_identity_changes_miss() {
        let root = temporary_root();
        let context = json!({"model":"mock","prompt":"Exact brief","schema":{},"scope":[0,2],"revision":"pinned"});
        let key = digest(&context).unwrap();
        store_plan(root.path(), &key, &plan()).unwrap();
        assert_eq!(load_plan(root.path(), &key).unwrap().unwrap().commands, plan().commands);
        for field in ["model", "prompt", "schema", "scope", "revision"] {
            let mut changed = context.clone();
            changed[field] = json!("changed");
            let changed = digest(&changed).unwrap();
            assert_ne!(changed, key);
            assert!(load_plan(root.path(), &changed).unwrap().is_none());
        }
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 2).write_to(&mut encoded, image::ImageFormat::Png).unwrap();
        let image = GeneratedImage { bytes: encoded.into_inner(), filename: "generated.png".into(), mime_type: "image/png".into() };
        store_image(root.path(), &key, &image).unwrap();
        let cached = load_image(root.path(), &key).unwrap().unwrap();
        assert_eq!(cached.bytes, image.bytes);
        assert_eq!(cached.filename, image.filename);
        assert_eq!(cached.mime_type, image.mime_type);
    }

    #[test]
    fn permissions_and_invalid_keys_cannot_write_outside_the_cache() {
        let allowed = temporary_root();
        let outside = temporary_root();
        let roots = file_access::AutomationRoots::new(Some(allowed.path()), Some(allowed.path())).unwrap().unwrap();
        file_access::confine(Some(&roots), || {
            assert!(store_plan(outside.path(), &key(), &plan()).is_err());
            assert!(load_plan(outside.path(), &key()).is_err());
            for bad in ["../escape", "a/b", "..", &"a".repeat(63), &"z".repeat(64)] {
                assert!(store_plan(allowed.path(), bad, &plan()).is_err());
                assert!(load_image(allowed.path(), bad).is_err());
            }
            store_plan(allowed.path(), &key(), &plan()).unwrap();
        });
        let read_only = file_access::AutomationRoots::new(Some(allowed.path()), None).unwrap().unwrap();
        file_access::confine(Some(&read_only), || assert!(store_plan(allowed.path(), &key(), &plan()).is_err()));
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(store_plan(&allowed.path().join(".."), &key(), &plan()).is_err());
    }

    #[test]
    fn corrupt_or_tampered_entries_return_errors_instead_of_plans() {
        let root = temporary_root();
        let path = entry_path(root.path(), &key(), "plan");
        std::fs::write(&path, b"not JSON").unwrap();
        assert!(load_plan(root.path(), &key()).unwrap_err().contains("corrupt"));
        store_plan(root.path(), &key(), &plan()).unwrap();
        let mut record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        record["payload"]["title"] = json!("Tampered");
        std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
        assert!(load_plan(root.path(), &key()).unwrap_err().contains("checksum"));
    }

    #[test]
    fn quota_evicts_oldest_entries_to_85_percent_and_declines_oversized_single_write() {
        let root = temporary_root();
        let filler = root.path().join("existing.plan.json");
        std::fs::File::create(&filler).unwrap().set_len(MAX_CACHE_BYTES).unwrap();
        store_plan(root.path(), &key(), &plan()).unwrap();
        assert!(!filler.exists(), "Old full-quota cache entry must be evicted to admit fresh plan");
        assert!(load_plan(root.path(), &key()).unwrap().is_some());

        // An impossible single write larger than MAX_CACHE_BYTES declines without deleting valid cached entries.
        let oversized = vec![b'x'; (MAX_CACHE_BYTES as usize) + 1];
        assert!(!write_cache_file(root.path(), &root.path().join("oversized.bin"), &oversized).unwrap());
        assert!(load_plan(root.path(), &key()).unwrap().is_some());
    }

    #[test]
    fn wave2_lru_eviction_handles_more_than_max_entries_and_preserves_rejected_diagnostic() {
        let root = temporary_root();
        store_rejected_response(root.path(), "initial diagnostic").unwrap();
        let sample = plan();
        let total_writes = MAX_ENTRIES + 24;
        let base_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let mut keys = Vec::with_capacity(total_writes);
        for index in 0..total_writes {
            let k = format!("{index:064x}");
            store_plan(root.path(), &k, &sample).unwrap();
            let p = entry_path(root.path(), &k, "plan");
            let mtime = base_time + std::time::Duration::from_secs(index as u64);
            std::fs::File::options().write(true).open(&p).unwrap().set_times(std::fs::FileTimes::new().set_modified(mtime)).unwrap();
            keys.push(k);
        }
        // Diagnostic file can still overwrite cleanly and is never evicted.
        let diag_path = store_rejected_response(root.path(), "updated diagnostic after eviction").unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(diag_path).unwrap(), "updated diagnostic after eviction");

        // Oldest entries were evicted; newest entries remain loadable and total cache entries stay <= MAX_ENTRIES.
        assert!(load_plan(root.path(), &keys[0]).unwrap().is_none(), "Oldest entry must be evicted");
        assert!(load_plan(root.path(), keys.last().unwrap()).unwrap().is_some(), "Newest entry must remain cached");
        let plan_files = std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".plan.json"))
            .count();
        assert!(plan_files <= MAX_ENTRIES, "plan_files={plan_files}");
    }

    #[test]
    fn ai21_rejected_response_is_bounded_replaced_and_never_a_validated_plan() {
        let root = temporary_root();
        let original = "{invalid private response}".repeat(800);
        let path = store_rejected_response(root.path(), &original).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(load_plan(root.path(), &key()).unwrap().is_none());
        let valid = plan();
        store_plan(root.path(), &key(), &valid).unwrap();
        store_rejected_response(root.path(), "نص تشخيصي جديد").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "نص تشخيصي جديد");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
        assert_eq!(serde_json::to_value(load_plan(root.path(), &key()).unwrap().unwrap()).unwrap(), serde_json::to_value(valid).unwrap());
        assert!(store_rejected_response(root.path(), &"x".repeat(4 * 1024 * 1024 + 1)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "نص تشخيصي جديد");
    }

    #[test]
    fn invalid_plan_bounds_and_deep_hash_inputs_are_rejected() {
        let root = temporary_root();
        let mut invalid = plan();
        invalid.commands.clear();
        assert!(store_plan(root.path(), &key(), &invalid).is_err());
        invalid = plan();
        invalid.commands.resize(513, invalid.commands[0].clone());
        assert!(store_plan(root.path(), &key(), &invalid).is_err());
        let mut nested = json!(0);
        for _ in 0..66 {
            nested = json!([nested]);
        }
        assert!(digest(&nested).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_link_roots_and_entries_are_refused() {
        use std::os::unix::fs::symlink;
        let root = temporary_root();
        let outside = temporary_root();
        symlink(outside.path(), root.path().join("linked")).unwrap();
        assert!(store_plan(&root.path().join("linked"), &key(), &plan()).is_err());
        let target = outside.path().join("entry");
        std::fs::write(&target, b"outside").unwrap();
        symlink(&target, entry_path(root.path(), &key(), "plan")).unwrap();
        assert!(load_plan(root.path(), &key()).is_err());
        assert!(store_plan(root.path(), &key(), &plan()).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"outside");
    }
}
