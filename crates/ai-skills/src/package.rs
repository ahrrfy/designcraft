use crate::{NativeManifest, Result, SkillError};
use designcraft_ai_types::EngineAction;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path};

pub type PackageFiles = BTreeMap<String, Vec<u8>>;
pub(crate) const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_PACKAGE: u64 = 32 * 1024 * 1024;
const MAX_FILES: usize = 256;
const MAX_DEPTH: usize = 8;
const MAX_ACTIONS: usize = 4096;

pub(crate) struct ParsedPackage {
    pub name: String,
    pub description: String,
    pub manifest: NativeManifest,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: serde_yaml::Value,
}

pub(crate) fn reject_symlink(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(SkillError::Invalid("symbolic links are not allowed".into()));
    }
    // Windows junctions and other reparse points can redirect directory traversal.
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(SkillError::Invalid("reparse points are not allowed".into()));
        }
    }
    Ok(())
}

pub(crate) fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    for ancestor in path.ancestors().skip(1).filter(|p| p.exists()) {
        reject_symlink(ancestor)?;
    }
    reject_symlink(path)?;
    let file = File::open(path)?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > max {
        return Err(SkillError::Invalid("file type or size limit".into()));
    }
    let mut bytes = Vec::new();
    file.take(max.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(SkillError::Invalid("file size limit".into()));
    }
    Ok(bytes)
}

pub(crate) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || id.starts_with('-')
        || id.ends_with('-')
        || id.contains("--")
    {
        return Err(SkillError::Invalid("skill id must use lowercase letters, digits and single hyphens".into()));
    }
    reject_windows_name(id)
}

fn reject_windows_name(name: &str) -> Result<()> {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5",
        "LPT6", "LPT7", "LPT8", "LPT9",
    ]
    .contains(&stem.as_str())
    {
        return Err(SkillError::Invalid("reserved device filename".into()));
    }
    Ok(())
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() || path.len() > 240 || path.contains(['\\', ':', '\0']) || path.starts_with('/') || path.ends_with('/') {
        return Err(SkillError::Invalid("unsafe package path".into()));
    }
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() > MAX_DEPTH
        || parts.iter().any(|p| p.is_empty() || *p == "." || *p == ".." || p.ends_with(['.', ' ']) || p.chars().any(char::is_control))
    {
        return Err(SkillError::Invalid("unsafe package path component".into()));
    }
    for part in parts {
        reject_windows_name(part)?;
    }
    if path != "SKILL.md" && path != "designcraft.json" && !["references/", "assets/", "templates/"].iter().any(|prefix| path.starts_with(prefix)) {
        return Err(SkillError::Unsupported("only SKILL.md, designcraft.json, references, assets and templates are supported".into()));
    }
    let extension = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if !["md", "txt", "json", "csv", "png", "jpg", "jpeg", "webp", "svg", "pdf"].contains(&extension.as_str()) {
        return Err(SkillError::Unsupported("executable or unsupported package asset".into()));
    }
    Ok(())
}

pub(crate) fn validate_package_path(path: &str) -> Result<()> {
    validate_path(path)
}

pub(crate) fn instructions(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes).map_err(|_| SkillError::Invalid("SKILL.md must be UTF-8".into()))?;
    let normalized = text.replace("\r\n", "\n");
    let rest = normalized.strip_prefix("---\n").ok_or_else(|| SkillError::Invalid("SKILL.md YAML frontmatter is required".into()))?;
    let (_, instructions) = rest.split_once("\n---\n").ok_or_else(|| SkillError::Invalid("unclosed YAML frontmatter".into()))?;
    if instructions.trim().is_empty() {
        return Err(SkillError::Invalid("empty skill instructions".into()));
    }
    Ok(instructions.trim().into())
}

pub(crate) fn validate_files(files: &PackageFiles) -> Result<()> {
    if files.is_empty() || files.len() > MAX_FILES {
        return Err(SkillError::Invalid("package file count limit".into()));
    }
    let mut total = 0u64;
    let mut folded = std::collections::BTreeSet::new();
    for (path, bytes) in files {
        validate_path(path)?;
        if !folded.insert(path.to_lowercase()) {
            return Err(SkillError::Invalid("case-colliding package files".into()));
        }
        total = total.checked_add(bytes.len() as u64).ok_or_else(|| SkillError::Invalid("package size overflow".into()))?;
        if bytes.len() as u64 > MAX_FILE || total > MAX_PACKAGE {
            return Err(SkillError::Invalid("package size limit".into()));
        }
    }
    Ok(())
}

pub(crate) fn parse(files: &PackageFiles) -> Result<ParsedPackage> {
    let skill_bytes = files.get("SKILL.md").ok_or_else(|| SkillError::Invalid("SKILL.md is required".into()))?;
    if skill_bytes.len() > 256 * 1024 {
        return Err(SkillError::Invalid("SKILL.md size limit".into()));
    }
    let text = std::str::from_utf8(skill_bytes).map_err(|_| SkillError::Invalid("SKILL.md must be UTF-8".into()))?;
    let normalized = text.replace("\r\n", "\n");
    let rest = normalized.strip_prefix("---\n").ok_or_else(|| SkillError::Invalid("SKILL.md YAML frontmatter is required".into()))?;
    let (yaml, instructions) = rest.split_once("\n---\n").ok_or_else(|| SkillError::Invalid("unclosed YAML frontmatter".into()))?;
    if yaml.len() > 16 * 1024 {
        return Err(SkillError::Invalid("frontmatter size limit".into()));
    }
    let front: Frontmatter = serde_yaml::from_str(yaml)?;
    validate_id(&front.name)?;
    if front.description.trim().is_empty() || front.description.len() > 2048 || instructions.trim().is_empty() {
        return Err(SkillError::Invalid("description and instructions are required".into()));
    }
    let mut manifest: NativeManifest = if let Some(bytes) = files.get("designcraft.json") {
        if bytes.len() > 256 * 1024 {
            return Err(SkillError::Invalid("manifest size limit".into()));
        }
        serde_json::from_slice(bytes)?
    } else {
        NativeManifest {
            schema_version: 1,
            skill_id: front.name.clone(),
            required_commands: Vec::new(),
            references: Vec::new(),
            templates: Vec::new(),
            sample_template: None,
            external_tools: vec!["unadapted-agent-skill".into()],
            compatibility_notes: "Adapt to native DesignCraft commands and add a reviewable sample before approval.".into(),
            design_profile: None,
        }
    };
    validate_id(&manifest.skill_id)?;
    if manifest.schema_version != 1 || manifest.skill_id != front.name {
        return Err(SkillError::Unsupported("manifest version or skill identity mismatch".into()));
    }
    if let Some(profile) = &manifest.design_profile {
        profile.validate().map_err(SkillError::Invalid)?;
        for family in &profile.page_families {
            if !manifest.templates.contains(&family.template) {
                return Err(SkillError::Invalid("design page family refers to an undeclared template".into()));
            }
            let actions = parse_actions(files.get(&family.template).ok_or_else(|| SkillError::Invalid("missing design page template".into()))?)?;
            for slot in &family.text_slots {
                if !actions.get(slot.command).is_some_and(|a| {
                    a.command == "frame.create"
                        && a.params.get("text").is_some_and(|v| v.is_string())
                        && a.params.get("content").and_then(serde_json::Value::as_str) == Some("text")
                }) {
                    return Err(SkillError::Invalid("design text slot must name a native text-frame creation action".into()));
                }
            }
        }
    }
    if !front.allowed_tools.is_null() {
        // Preserve upstream tool requirements as an approval blocker; importing a
        // claimed native manifest cannot silently grant access to external tools.
        manifest.external_tools.push("upstream-allowed-tools".into());
    }
    if manifest.required_commands.len() > 128
        || manifest.references.len() > MAX_FILES
        || manifest.templates.len() > 32
        || manifest.external_tools.len() > 128
    {
        return Err(SkillError::Invalid("manifest entry limit".into()));
    }
    for command in &manifest.required_commands {
        if command.is_empty() || command.len() > 128 || !command.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-') {
            return Err(SkillError::Invalid("invalid native command identifier".into()));
        }
    }
    for reference in &manifest.references {
        validate_path(reference)?;
        if !reference.starts_with("references/") || !files.contains_key(reference) {
            return Err(SkillError::Invalid("missing or invalid reference".into()));
        }
    }
    for template in &manifest.templates {
        validate_path(template)?;
        if !template.starts_with("templates/") || !template.ends_with(".json") {
            return Err(SkillError::Invalid("templates must be native JSON".into()));
        }
        parse_actions(files.get(template).ok_or_else(|| SkillError::Invalid("missing template".into()))?)?;
    }
    if let Some(sample) = &manifest.sample_template
        && !manifest.templates.contains(sample)
    {
        return Err(SkillError::Invalid("sample template is not declared".into()));
    }
    Ok(ParsedPackage { name: front.name, description: front.description, manifest })
}

pub(crate) fn parse_actions(bytes: &[u8]) -> Result<Vec<EngineAction>> {
    if bytes.len() > 1024 * 1024 {
        return Err(SkillError::Invalid("native template size limit".into()));
    }
    let actions: Vec<EngineAction> = serde_json::from_slice(bytes)?;
    if actions.is_empty()
        || actions.len() > MAX_ACTIONS
        || actions.iter().any(|a| a.command.is_empty() || a.command.len() > 128 || !a.params.is_object())
    {
        return Err(SkillError::Invalid("invalid native template actions".into()));
    }
    Ok(actions)
}

pub(crate) fn read_folder(root: &Path) -> Result<PackageFiles> {
    for ancestor in root.ancestors().filter(|p| p.exists()) {
        reject_symlink(ancestor)?;
    }
    reject_symlink(root)?;
    if !root.is_dir() {
        return Err(SkillError::Invalid("package root is not a directory".into()));
    }
    let mut files = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut total = 0u64;
    let mut entries = 0usize;
    while let Some((directory, depth)) = stack.pop() {
        if depth >= MAX_DEPTH {
            return Err(SkillError::Invalid("package nesting limit".into()));
        }
        reject_symlink(&directory)?;
        for item in fs::read_dir(&directory)? {
            let item = item?;
            entries = entries.saturating_add(1);
            if entries > MAX_FILES * 2 {
                return Err(SkillError::Invalid("package entry limit".into()));
            }
            let path = item.path();
            reject_symlink(&path)?;
            if item.file_type()?.is_dir() {
                stack.push((path, depth.saturating_add(1)));
                continue;
            }
            let relative = path.strip_prefix(root).map_err(|_| SkillError::Invalid("path escapes package".into()))?;
            let parts: Option<Vec<_>> = relative
                .components()
                .map(|component| match component {
                    Component::Normal(p) => p.to_str(),
                    _ => None,
                })
                .collect();
            let name = parts.ok_or_else(|| SkillError::Invalid("non-UTF8 or unsafe filename".into()))?.join("/");
            validate_path(&name)?;
            let bytes = read_bounded(&path, MAX_FILE)?;
            total = total.checked_add(bytes.len() as u64).ok_or_else(|| SkillError::Invalid("package size overflow".into()))?;
            if total > MAX_PACKAGE || files.len() >= MAX_FILES {
                return Err(SkillError::Invalid("package size or count limit".into()));
            }
            files.insert(name, bytes);
        }
    }
    validate_files(&files)?;
    Ok(files)
}

/// Decode the evidence with a strict allocation limit; a filename or signature
/// alone is not proof that a reviewable render exists.
pub(crate) fn validate_sample_png(bytes: &[u8]) -> Result<()> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    decoder.set_limits(png::Limits { bytes: 64 * 1024 * 1024 });
    let mut reader = decoder.read_info().map_err(|e| SkillError::Invalid(format!("sample PNG: {e}")))?;
    let size = reader.output_buffer_size().ok_or_else(|| SkillError::Invalid("sample PNG output size".into()))?;
    if size == 0 || size > 64 * 1024 * 1024 {
        return Err(SkillError::Invalid("sample PNG output size limit".into()));
    }
    let mut buffer = vec![0; size];
    reader.next_frame(&mut buffer).map_err(|e| SkillError::Invalid(format!("sample PNG pixels: {e}")))?;
    Ok(())
}

pub(crate) fn read_zip(path: &Path) -> Result<PackageFiles> {
    let compressed = read_bounded(path, MAX_PACKAGE)?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(compressed))?;
    if archive.len() > MAX_FILES * 2 {
        return Err(SkillError::Invalid("ZIP entry limit".into()));
    }
    let mut total = 0u64;
    let mut files = BTreeMap::new();
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().to_string();
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                return Err(SkillError::Invalid("ZIP links and special files are forbidden".into()));
            }
        }
        if entry.is_dir() {
            // Directory entries are never extracted but must still be safe.
            let directory = name.trim_end_matches('/');
            if directory.is_empty()
                || directory.split('/').any(|p| p.is_empty() || p == "." || p == "..")
                || directory.contains(['\\', ':', '\0'])
                || directory.starts_with('/')
            {
                return Err(SkillError::Invalid("unsafe ZIP directory".into()));
            }
            continue;
        }
        validate_path(&name)?;
        total = total.checked_add(entry.size()).ok_or_else(|| SkillError::Invalid("ZIP size overflow".into()))?;
        if entry.size() > MAX_FILE || total > MAX_PACKAGE || files.len() >= MAX_FILES {
            return Err(SkillError::Invalid("ZIP expanded size limit".into()));
        }
        let mut bytes = Vec::new();
        entry.take(MAX_FILE.saturating_add(1)).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE {
            return Err(SkillError::Invalid("ZIP file size limit".into()));
        }
        if files.insert(name, bytes).is_some() {
            return Err(SkillError::Invalid("duplicate ZIP entry".into()));
        }
    }
    validate_files(&files)?;
    Ok(files)
}

pub(crate) fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub(crate) fn write_folder(root: &Path, files: &PackageFiles) -> Result<()> {
    validate_files(files)?;
    if root.exists() {
        return Err(SkillError::Invalid("destination already exists".into()));
    }
    // Check existing ancestors before creating directories, including Windows junctions.
    for ancestor in root.ancestors().filter(|p| p.exists()) {
        reject_symlink(ancestor)?;
    }
    fs::create_dir_all(root)?;
    for (name, bytes) in files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            reject_symlink(parent)?;
        }
        write_new(&path, bytes)?;
    }
    Ok(())
}

pub(crate) fn write_zip(destination: &Path, files: &PackageFiles) -> Result<()> {
    for ancestor in destination.ancestors().skip(1).filter(|p| p.exists()) {
        reject_symlink(ancestor)?;
    }
    let file = OpenOptions::new().write(true).create_new(true).open(destination)?;
    let mut archive = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in files {
        archive.start_file(name, options)?;
        archive.write_all(bytes)?;
    }
    archive.finish()?.sync_all()?;
    Ok(())
}
