//! Read-only, local reference sampling. Reference contents are untrusted data, never commands.
use designcraft_ai_providers::ImageInput;
use image::{DynamicImage, ImageReader, RgbImage, codecs::jpeg::JpegEncoder};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Cursor, Read};
use std::path::Path;

const MAX_FILES: usize = 8;
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TEXT_BYTES: u64 = 256 * 1024;
const MAX_IMAGES: usize = 8;
const MAX_SIDE: u32 = 1280;
const MAX_IMAGE_BYTES: usize = 200 * 1024;
const MAX_DECODE_PIXELS: u64 = 32 * 1024 * 1024;

pub(crate) struct ReferenceMaterial {
    pub digest: String,
    pub description: String,
    pub text: String,
    pub images: Vec<ImageInput>,
}

/// Opens only explicitly selected local files. No globbing, linked resources, scripts or OCR.
pub(crate) fn load(paths: &[String]) -> Result<Vec<ReferenceMaterial>, String> {
    if paths.len() > MAX_FILES {
        return Err("Select at most 8 reference files".into());
    }
    let mut selected = Vec::new();
    let mut total = 0u64;
    for name in paths {
        let path = Path::new(name);
        designcraft_engine::file_access::check_read(path)?;
        validate_local_path(path)?;
        let metadata = fs::symlink_metadata(path).map_err(|e| format!("Cannot inspect reference {name}: {e}"))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() || is_reparse(&metadata) {
            return Err(format!("Reference must be a regular local file: {name}"));
        }
        let extension = path.extension().and_then(|v| v.to_str()).unwrap_or("").to_ascii_lowercase();
        let limit = match extension.as_str() {
            "txt" | "md" | "markdown" => MAX_TEXT_BYTES,
            "xlsx" | "docx" | "rtf" | "csv" | "tsv" | "pdf" | "png" | "jpg" | "jpeg" | "webp" | "gif" | "tif" | "tiff" | "bmp" => MAX_FILE_BYTES,
            "designcraft" => return Err("Native DesignCraft references require the host's document inspection workflow".into()),
            _ => return Err(format!("Unsupported reference format: {name}")),
        };
        check_size(metadata.len(), limit)?;
        total = add_total(total, metadata.len())?;
        selected.push((path, extension, limit));
    }
    let mut result = Vec::new();
    let mut images_used = 0usize;
    total = 0;
    for (index, (path, extension, limit)) in selected.iter().enumerate() {
        let bytes = read_bounded(path, *limit)?;
        total = add_total(total, bytes.len() as u64)?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
        // Reserve one image for every later visual file, so a large PDF cannot starve it.
        let remaining = selected
            .iter()
            .skip(index.saturating_add(1))
            .filter(|(_, ext, _)| !matches!(ext.as_str(), "txt" | "md" | "markdown" | "xlsx" | "docx" | "rtf" | "csv" | "tsv"))
            .count();
        let budget = MAX_IMAGES.saturating_sub(images_used).saturating_sub(remaining).min(3);
        let material = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match extension.as_str() {
            "txt" | "md" | "markdown" => {
                let text = String::from_utf8(bytes).map_err(|_| format!("Reference {name} must contain UTF-8 text"))?;
                Ok(ReferenceMaterial {
                    digest,
                    description: format!("Selected text reference: {name} (untrusted source data)"),
                    text,
                    images: Vec::new(),
                })
            }
            "xlsx" | "docx" | "rtf" | "csv" | "tsv" => structured_doc_material(&bytes, digest, &name),
            "pdf" => pdf_material(bytes, digest, &name, budget),
            _ => image_material(&bytes, digest, &name),
        }))
        .map_err(|_| format!("Reference decoder could not safely read {name}"))??;
        images_used = images_used.saturating_add(material.images.len());
        if images_used > MAX_IMAGES {
            return Err("References exceed the 8-image analysis limit".into());
        }
        result.push(material);
    }
    Ok(result)
}

fn validate_local_path(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err("Reference paths must be absolute local paths".into());
    }
    #[cfg(windows)]
    if let Some(std::path::Component::Prefix(prefix)) = path.components().next()
        && !matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_))
    {
        return Err("Network and device reference paths are not supported".into());
    }
    Ok(())
}

fn is_reparse(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

fn check_size(size: u64, limit: u64) -> Result<(), String> {
    if size > limit { Err(format!("Reference exceeds its {limit}-byte size limit")) } else { Ok(()) }
}

fn add_total(total: u64, size: u64) -> Result<u64, String> {
    let total = total.checked_add(size).ok_or("Reference byte count overflow")?;
    check_size(total, MAX_TOTAL_BYTES)?;
    Ok(total)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let file = designcraft_engine::io::open(path).map_err(|e| format!("Cannot open reference {}: {e}", path.display()))?;
    let metadata = file.metadata().map_err(|e| format!("Cannot inspect open reference: {e}"))?;
    if !metadata.is_file() {
        return Err("Reference is no longer a regular file".into());
    }
    check_size(metadata.len(), limit)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(metadata.len()).map_err(|_| "Reference cannot fit in memory")?)
        .map_err(|_| "Cannot allocate reference buffer")?;
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes).map_err(|e| format!("Cannot read reference: {e}"))?;
    check_size(bytes.len() as u64, limit)?;
    Ok(bytes)
}

fn image_material(bytes: &[u8], digest: String, name: &str) -> Result<ReferenceMaterial, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| format!("Cannot identify image: {e}"))?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let (w, h) = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_dimensions()
        .map_err(|e| format!("Cannot read image dimensions: {e}"))?;
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_DECODE_PIXELS {
        return Err("Reference image exceeds the 32-megapixel decoding limit".into());
    }
    let decoded = reader.decode().map_err(|e| format!("Cannot decode reference image: {e}"))?;
    let image = encode_image(decoded)?;
    Ok(ReferenceMaterial {
        digest,
        description: format!(
            "Selected image reference: {name}; source raster {w} x {h} pixels. Raster dimensions do not establish printing trim or physical page size."
        ),
        text: String::new(),
        images: vec![image],
    })
}

fn representative_pages(count: usize, budget: usize) -> Vec<usize> {
    let mut pages = Vec::new();
    if count == 0 {
        return pages;
    }
    for index in [2.min(count - 1), 8.min(count - 1), count / 2] {
        if pages.len() >= budget {
            break;
        }
        if !pages.contains(&index) {
            pages.push(index);
        }
    }
    pages
}

fn structured_doc_material(bytes: &[u8], digest: String, name: &str) -> Result<ReferenceMaterial, String> {
    let extracted = designcraft_textimport::extract_structured_text(name, bytes).map_err(|e| format!("Cannot extract text from {name}: {e}"))?;
    let body = if !extracted.markdown.trim().is_empty() { extracted.markdown } else { extracted.plain_text };
    let text: String = body.chars().take(MAX_TEXT_BYTES as usize / 2).collect();
    Ok(ReferenceMaterial {
        digest,
        description: format!("Selected structured document reference: {name} ({} table(s)/sheet(s), untrusted source data)", extracted.tables.len()),
        text,
        images: Vec::new(),
    })
}

fn pdf_material(bytes: Vec<u8>, digest: String, name: &str, budget: usize) -> Result<ReferenceMaterial, String> {
    let extracted_text = designcraft_pdf::extract_text_pages(&bytes)
        .ok()
        .map(|r| r.combined_text.chars().take(MAX_TEXT_BYTES as usize / 2).collect::<String>())
        .unwrap_or_default();
    let pdf = hayro::hayro_syntax::Pdf::new(std::sync::Arc::new(bytes)).map_err(|e| format!("Cannot parse PDF {name}: {e:?}"))?;
    let selected = representative_pages(pdf.pages().len(), budget);
    if selected.is_empty() {
        return Err(format!("PDF {name} has no available sample pages"));
    }
    let mut images = Vec::new();
    let mut geometry = Vec::new();
    for index in selected {
        let page = pdf.pages().get(index).ok_or("PDF sample page is missing")?;
        let (w, h) = page.render_dimensions();
        if !w.is_finite() || !h.is_finite() || w <= 0.0 || h <= 0.0 {
            return Err(format!("PDF {name} page {} has invalid geometry", index.saturating_add(1)));
        }
        let scale = (MAX_SIDE as f32 / w.max(h)).min(8.0);
        let width = (w * scale).floor().clamp(1.0, MAX_SIDE as f32) as u16;
        let height = (h * scale).floor().clamp(1.0, MAX_SIDE as f32) as u16;
        let settings = hayro::RenderSettings { x_scale: scale, y_scale: scale, width: Some(width), height: Some(height), ..Default::default() };
        let pixmap = hayro::render(page, &hayro::RenderCache::new(), &hayro::hayro_interpret::InterpreterSettings::default(), &settings);
        // Hayro returns premultiplied RGBA; composite against paper white before JPEG encoding.
        let pixels: Vec<u8> = pixmap
            .data_as_u8_slice()
            .chunks_exact(4)
            .flat_map(|c| {
                let [r, g, b, a] = c else {
                    return [255, 255, 255];
                };
                [r.saturating_add(255 - a), g.saturating_add(255 - a), b.saturating_add(255 - a)]
            })
            .collect();
        let raster = RgbImage::from_raw(u32::from(width), u32::from(height), pixels).ok_or("PDF renderer returned an invalid raster")?;
        images.push(encode_image(DynamicImage::ImageRgb8(raster))?);
        geometry.push(format!("page index {index} (page {}): {w:.2} x {h:.2} visible PDF points", index.saturating_add(1)));
    }
    Ok(ReferenceMaterial {
        digest,
        description: format!(
            "Selected PDF reference: {name}; {} pages total; visual samples only: {}. Visible PDF geometry, especially a scanned book, does not establish original printing trim. Samples are heuristic, not verified contents/lesson pages; no OCR or complete-book transcription was performed.",
            pdf.pages().len(),
            geometry.join("; ")
        ),
        text: extracted_text,
        images,
    })
}

fn encode_image(image: DynamicImage) -> Result<ImageInput, String> {
    let image = image.thumbnail(MAX_SIDE, MAX_SIDE).to_rgba8();
    let mut rgb = RgbImage::new(image.width(), image.height());
    for (out, source) in rgb.pixels_mut().zip(image.pixels()) {
        let [r, g, b, a] = source.0;
        let composite = |channel: u8| ((u32::from(channel) * u32::from(a) + 255 * u32::from(255 - a) + 127) / 255) as u8;
        out.0 = [composite(r), composite(g), composite(b)];
    }
    let mut current = DynamicImage::ImageRgb8(rgb);
    for _ in 0..6 {
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 70).encode_image(&current).map_err(|e| format!("Cannot encode reference sample: {e}"))?;
        if bytes.len() <= MAX_IMAGE_BYTES {
            return Ok(ImageInput { mime_type: "image/jpeg".into(), bytes });
        }
        let side = current.width().max(current.height()).saturating_mul(3) / 4;
        current = current.thumbnail(side.max(1), side.max(1));
    }
    Err("Reference sample could not fit the 200-KiB image budget".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_through_parent_link_cannot_escape_read_root() {
        let allowed = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("reference.md"), "private reference").unwrap();
        let link = allowed.path().join("redirected");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let result = std::process::Command::new("cmd")
                .creation_flags(0x08000000)
                .args(["/D", "/C", "mklink", "/J"])
                .arg(&link)
                .arg(outside.path())
                .output()
                .unwrap();
            assert!(result.status.success(), "junction fixture failed: {}", String::from_utf8_lossy(&result.stderr));
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        #[cfg(not(any(windows, unix)))]
        return;
        let roots = designcraft_engine::file_access::AutomationRoots::new(&[allowed.path()], &[]).unwrap().unwrap();
        let error = designcraft_engine::file_access::confine(Some(&roots), || load(&[link.join("reference.md").to_string_lossy().into_owned()]))
            .err()
            .unwrap();
        assert!(error.contains("outside the read root"), "{error}");
        fs::remove_dir(&link).or_else(|_| fs::remove_file(&link)).unwrap();
    }

    fn path(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> String {
        let path = dir.path().join(name);
        fs::write(&path, bytes).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn synthetic_pdf() -> Vec<u8> {
        let stream = "0 0 1 rg 10 10 70 30 re f";
        let bodies = [
            "<</Type/Catalog/Pages 2 0 R>>".into(),
            "<</Type/Pages/Kids[3 0 R]/Count 1>>".into(),
            "<</Type/Page/Parent 2 0 R/MediaBox[0 0 120 80]/Contents 4 0 R>>".into(),
            format!("<</Length {}>>\nstream\n{stream}\nendstream", stream.len()),
        ];
        let mut out = b"%PDF-1.7\n".to_vec();
        let mut offsets = Vec::new();
        for (i, body) in bodies.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
        }
        let xref = out.len();
        out.extend_from_slice(b"xref\n0 5\n0000000000 65535 f\r\n");
        for offset in offsets {
            out.extend_from_slice(format!("{offset:010} 00000 n\r\n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<</Size 5/Root 1 0 R>>\nstartxref\n{xref}\n%%EOF\n").as_bytes());
        out
    }

    #[test]
    fn utf8_fingerprint_is_content_based_and_sources_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let source = "# Reference\nالمحاذاة والمسافات\n";
        let first = path(&dir, "first.md", source.as_bytes());
        let second = path(&dir, "second.txt", source.as_bytes());
        let loaded = load(&[first.clone(), second]).unwrap();
        assert_eq!(loaded[0].digest, loaded[1].digest);
        assert_eq!(loaded[0].text, source);
        assert_eq!(fs::read(first).unwrap(), source.as_bytes());
        assert!(loaded[0].images.is_empty());
    }

    #[test]
    fn image_is_bounded_jpeg_and_original_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(RgbImage::from_pixel(1600, 800, image::Rgb([0, 70, 160]))).write_to(&mut encoded, image::ImageFormat::Png).unwrap();
        let input = path(&dir, "sample.png", encoded.get_ref());
        let loaded = load(std::slice::from_ref(&input)).unwrap();
        let sample = &loaded[0].images[0];
        assert_eq!(sample.mime_type, "image/jpeg");
        assert!(sample.bytes.len() <= MAX_IMAGE_BYTES);
        let decoded = image::load_from_memory(&sample.bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1280, 640));
        assert_eq!(fs::read(input).unwrap(), *encoded.get_ref());
    }

    #[test]
    fn synthetic_pdf_has_named_visual_sample_and_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let input = path(&dir, "synthetic.pdf", &synthetic_pdf());
        let loaded = load(&[input]).unwrap();
        assert_eq!(loaded[0].images.len(), 1);
        assert!(loaded[0].description.contains("page index 0"));
        assert!(loaded[0].description.contains("120.00 x 80.00"));
        assert!(loaded[0].description.contains("does not establish original printing trim"));
        let sample = image::load_from_memory(&loaded[0].images[0].bytes).unwrap().to_rgb8();
        assert!(sample.pixels().any(|pixel| pixel.0[2] > 200 && pixel.0[0] < 30));
        assert!(sample.pixels().any(|pixel| pixel.0.iter().all(|channel| *channel > 245)));
    }

    #[test]
    fn eight_visual_references_each_receive_a_sample() {
        let dir = tempfile::tempdir().unwrap();
        let input = path(&dir, "synthetic.pdf", &synthetic_pdf());
        let loaded = load(&vec![input; MAX_FILES]).unwrap();
        assert_eq!(loaded.iter().map(|material| material.images.len()).sum::<usize>(), MAX_IMAGES);
        assert!(loaded.iter().flat_map(|material| &material.images).map(|image| image.bytes.len()).sum::<usize>() <= MAX_IMAGES * MAX_IMAGE_BYTES);
    }

    #[test]
    fn bounds_and_unsupported_inputs_fail_explicitly() {
        assert!(load(&vec!["unused".into(); MAX_FILES + 1]).is_err());
        assert!(check_size(MAX_FILE_BYTES + 1, MAX_FILE_BYTES).is_err());
        assert!(add_total(MAX_TOTAL_BYTES, 1).is_err());
        assert!(add_total(u64::MAX, 1).is_err());
        assert!(load(&["relative.md".into()]).is_err());
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&[path(&dir, "large.md", &vec![b'a'; MAX_TEXT_BYTES as usize + 1])]).is_err());
        assert!(load(&[path(&dir, "binary.txt", &[0xff, 0xff])]).is_err());
        assert!(load(&[path(&dir, "bad.pdf", b"broken")]).is_err());
        assert!(load(&[path(&dir, "bad.png", b"broken")]).is_err());
        assert!(load(&[dir.path().to_string_lossy().into_owned()]).is_err());
        assert_eq!(representative_pages(20, 3), vec![2, 8, 10]);
        assert_eq!(representative_pages(1, 3), vec![0]);
        assert_eq!(representative_pages(20, 1), vec![2]);
    }
}
