//! Local Agent Skills packages with explicit review, immutable revisions and pinned tasks.
//!
//! Package content is data, never executable code. `SKILL.md` uses Agent Skills YAML
//! frontmatter; `designcraft.json` adapts it to native DesignCraft commands. Importing,
//! editing or generating a package creates a draft. Activation requires exact native sample
//! evidence, either human review or automatic native validation recorded separately.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod builtin;
mod educational_style;
mod package;

use designcraft_ai_types::EngineAction;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

pub use package::PackageFiles;

const MAX_INDEX: u64 = 8 * 1024 * 1024;
const MAX_CACHE_TEXT: usize = 256 * 1024;
const MAX_REVISIONS: usize = 4096;
const MAX_SNAPSHOT_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("skill I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("skill JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("skill YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("skill ZIP: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("invalid skill: {0}")]
    Invalid(String),
    #[error("skill not found: {0}")]
    NotFound(String),
    #[error("skill integrity mismatch: {0}")]
    Integrity(String),
    #[error("unsupported skill: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, SkillError>;

/// Text formats eligible for progressive model context. Binary references may
/// be analyzed and cached separately, but are never implicitly loaded as text.
pub const SUPPORTED_TEXT_REFERENCE_EXTENSIONS: &[&str] = &["md", "txt", "json", "csv"];

pub fn is_supported_text_reference(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SUPPORTED_TEXT_REFERENCE_EXTENSIONS.iter().any(|supported| ext.eq_ignore_ascii_case(supported)))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RevisionStatus {
    Draft,
    Approved,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// Human-readable source, without secrets or source documents.
    pub source: String,
    pub author: Option<String>,
    pub license: Option<String>,
    pub source_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeManifest {
    pub schema_version: u32,
    pub skill_id: String,
    #[serde(default)]
    pub required_commands: Vec<String>,
    #[serde(default)]
    pub references: Vec<String>,
    #[serde(default)]
    pub templates: Vec<String>,
    /// A template containing a nonempty JSON array of native EngineActions.
    pub sample_template: Option<String>,
    #[serde(default)]
    pub external_tools: Vec<String>,
    #[serde(default)]
    pub compatibility_notes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_profile: Option<designcraft_ai_types::DesignProfile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillRevision {
    pub skill_id: String,
    /// Content hash, also the immutable revision identifier.
    pub revision: String,
    pub name: String,
    pub description: String,
    pub status: RevisionStatus,
    pub provenance: Provenance,
    pub manifest: NativeManifest,
    pub file_hashes: BTreeMap<String, String>,
    pub approval: Option<ApprovalRecord>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalRecord {
    #[serde(default)]
    pub automatic: bool,
    pub reviewer: String,
    pub sample_sha256: String,
    pub sample_format: String,
    pub notes: String,
}

/// The host supplies evidence after executing the sample on a scratch document
/// and presenting its render to the human. The registry checks the exact actions,
/// actual artifact bytes and declared capabilities before recording approval.
pub struct ApprovalEvidence {
    pub reviewer: String,
    pub sample_artifact: PathBuf,
    pub executed_actions: Vec<EngineAction>,
    pub supported_commands: BTreeSet<String>,
    pub notes: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub skill_id: String,
    pub name: String,
    pub description: String,
    pub active_revision: Option<String>,
    pub enabled: bool,
    pub revisions: Vec<SkillRevision>,
}

/// Owned, immutable task input. Later approval, edits, rollback or disabling do
/// not mutate a running task's snapshot. A disabled skill cannot start a new task.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillSnapshot {
    pub skill_id: String,
    pub revision: String,
    pub instructions: String,
    pub references: BTreeMap<String, String>,
    pub reference_hashes: BTreeMap<String, String>,
    pub templates: BTreeMap<String, Vec<EngineAction>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_profile: Option<designcraft_ai_types::DesignProfile>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedAnalysis {
    pub reference_sha256: String,
    pub analyzer: String,
    pub text: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Index {
    entries: BTreeMap<String, CatalogEntry>,
    analysis_cache: BTreeMap<String, CachedAnalysis>,
}

/// One local writer per catalog directory. Keep an instance owned by the host
/// worker; route mutations through that worker instead of opening multiple writers.
pub struct SkillRegistry {
    root: PathBuf,
    index: Index,
}

impl SkillRegistry {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        for ancestor in root.ancestors().filter(|p| p.exists()) {
            package::reject_symlink(ancestor)?;
        }
        fs::create_dir_all(&root)?;
        package::reject_symlink(&root)?;
        let index_path = root.join("catalog.json");
        let index: Index =
            if index_path.exists() { serde_json::from_slice(&package::read_bounded(&index_path, MAX_INDEX)?)? } else { Index::default() };
        for (id, entry) in &index.entries {
            package::validate_id(id)?;
            if entry.skill_id != *id || entry.revisions.len() > MAX_REVISIONS {
                return Err(SkillError::Integrity("catalog identity or revision limit".into()));
            }
            for revision in &entry.revisions {
                validate_revision_id(&revision.revision)?;
                if revision.skill_id != *id || revision.manifest.skill_id != *id {
                    return Err(SkillError::Integrity("revision identity".into()));
                }
            }
            if let Some(active) = &entry.active_revision
                && !entry.revisions.iter().any(|r| r.revision == *active && r.status == RevisionStatus::Approved && r.approval.is_some())
            {
                return Err(SkillError::Integrity("active revision is not approved".into()));
            }
        }
        Ok(Self { root, index })
    }

    pub fn catalog(&self, query: &str) -> Vec<CatalogEntry> {
        let query = query.to_lowercase();
        self.index
            .entries
            .values()
            .filter(|e| query.is_empty() || format!("{} {} {}", e.skill_id, e.name, e.description).to_lowercase().contains(&query))
            .cloned()
            .collect()
    }

    pub fn revision(&self, skill_id: &str, revision: &str) -> Result<&SkillRevision> {
        self.index
            .entries
            .get(skill_id)
            .and_then(|e| e.revisions.iter().find(|r| r.revision == revision))
            .ok_or_else(|| SkillError::NotFound(format!("{skill_id}@{revision}")))
    }

    /// Load a verified package for human inspection or editing. Editing these
    /// owned bytes cannot mutate the stored revision; import changes as a draft.
    pub fn review_files(&self, skill_id: &str, revision: &str) -> Result<PackageFiles> {
        self.verified_files(self.revision(skill_id, revision)?)
    }

    /// Verified draft or approved template data for the native host's isolated
    /// compatibility review. This never executes commands or grants approval.
    pub fn review_templates(&self, skill_id: &str, revision: &str) -> Result<BTreeMap<String, Vec<EngineAction>>> {
        let record = self.revision(skill_id, revision)?;
        if record.manifest.templates.len() > 16 {
            return Err(SkillError::Unsupported("skill review supports at most 16 native templates".into()));
        }
        let files = self.verified_files(record)?;
        let mut templates = BTreeMap::new();
        let mut total = 0usize;
        for name in &record.manifest.templates {
            let actions = package::parse_actions(files.get(name).ok_or_else(|| SkillError::Integrity(name.clone()))?)?;
            total = total.checked_add(actions.len()).ok_or_else(|| SkillError::Invalid("template review action overflow".into()))?;
            if total > 4096 {
                return Err(SkillError::Unsupported("skill review supports at most 4096 actions across all templates".into()));
            }
            if templates.insert(name.clone(), actions).is_some() {
                return Err(SkillError::Invalid("duplicate native review template".into()));
            }
        }
        Ok(templates)
    }

    pub fn import_folder(&mut self, path: impl AsRef<Path>, provenance: Provenance) -> Result<SkillRevision> {
        self.import_files(package::read_folder(path.as_ref())?, provenance)
    }

    pub fn import_zip(&mut self, path: impl AsRef<Path>, provenance: Provenance) -> Result<SkillRevision> {
        self.import_files(package::read_zip(path.as_ref())?, provenance)
    }

    /// Also the edit/generate API: provide a complete replacement package; it is
    /// always a new draft and does not replace the currently approved revision.
    pub fn import_files(&mut self, files: PackageFiles, provenance: Provenance) -> Result<SkillRevision> {
        package::validate_files(&files)?;
        let parsed = package::parse(&files)?;
        let hashes: BTreeMap<String, String> = files.iter().map(|(p, bytes)| (p.clone(), sha256(bytes))).collect();
        let revision_id = package_hash(&hashes);
        let skill_id = parsed.manifest.skill_id.clone();
        // Idempotent imports never change the review state of existing content.
        if let Ok(existing) = self.revision(&skill_id, &revision_id) {
            return Ok(existing.clone());
        }
        let revision = SkillRevision {
            skill_id: skill_id.clone(),
            revision: revision_id.clone(),
            name: parsed.name,
            description: parsed.description,
            status: RevisionStatus::Draft,
            provenance,
            manifest: parsed.manifest,
            file_hashes: hashes,
            approval: None,
        };
        if self.index.entries.values().map(|e| e.revisions.len()).sum::<usize>() >= MAX_REVISIONS {
            return Err(SkillError::Invalid("catalog revision limit reached".into()));
        }
        let revision_dir = self.revision_dir(&skill_id, &revision_id)?;
        if revision_dir.exists() {
            return Err(SkillError::Integrity("unindexed revision directory already exists".into()));
        }
        package::write_folder(&revision_dir, &files)?;
        let mut next = self.index.clone();
        let entry = next.entries.entry(skill_id.clone()).or_insert_with(|| CatalogEntry {
            skill_id,
            name: revision.name.clone(),
            description: revision.description.clone(),
            active_revision: None,
            enabled: true,
            revisions: Vec::new(),
        });
        entry.revisions.push(revision.clone());
        self.persist(next)?;
        Ok(revision)
    }

    /// Fetch the draft sample actions for scratch execution and visual review.
    pub fn sample_actions(&self, skill_id: &str, revision: &str) -> Result<Vec<EngineAction>> {
        let record = self.revision(skill_id, revision)?;
        let files = self.verified_files(record)?;
        let template = record.manifest.sample_template.as_ref().ok_or_else(|| SkillError::Unsupported("native sample template is missing".into()))?;
        package::parse_actions(files.get(template).ok_or_else(|| SkillError::Integrity(template.clone()))?)
    }

    /// Explicit human approval after review. An existing approved revision is
    /// immutable; importing identical bytes is idempotent, never a review bypass.
    pub fn approve(&mut self, skill_id: &str, revision: &str, evidence: ApprovalEvidence) -> Result<()> {
        self.activate(skill_id, revision, evidence, false)
    }

    /// Host-only local validation, never a claim that a human reviewed the sample.
    pub fn activate_automatically(&mut self, skill_id: &str, revision: &str, mut evidence: ApprovalEvidence) -> Result<()> {
        evidence.reviewer = "DesignCraft native validator".into();
        evidence.notes = "Automatic native template execution, rendered sample and print preflight; no human approval claimed.".into();
        self.activate(skill_id, revision, evidence, true)
    }

    fn activate(&mut self, skill_id: &str, revision: &str, evidence: ApprovalEvidence, automatic: bool) -> Result<()> {
        let record = self.revision(skill_id, revision)?.clone();
        if record.status != RevisionStatus::Draft && (automatic || !record.approval.as_ref().is_some_and(|approval| approval.automatic)) {
            return Err(SkillError::Invalid("revision is already approved".into()));
        }
        if evidence.reviewer.trim().is_empty() || evidence.notes.trim().is_empty() {
            return Err(SkillError::Invalid("human reviewer and review notes are required".into()));
        }
        if !record.manifest.external_tools.is_empty() || record.manifest.required_commands.is_empty() {
            return Err(SkillError::Unsupported("adapt external tools to native commands before approval".into()));
        }
        if record.manifest.required_commands.iter().any(|cmd| !evidence.supported_commands.contains(cmd)) {
            return Err(SkillError::Unsupported("required native command is unavailable".into()));
        }
        let expected = self.sample_actions(skill_id, revision)?;
        if expected.is_empty() || expected != evidence.executed_actions {
            return Err(SkillError::Invalid("sample execution does not match the revision template".into()));
        }
        for actions in self.review_templates(skill_id, revision)?.values() {
            if actions.iter().any(|a| !record.manifest.required_commands.contains(&a.command) || !evidence.supported_commands.contains(&a.command)) {
                return Err(SkillError::Unsupported("template contains an undeclared or unavailable command".into()));
            }
        }
        let artifact = package::read_bounded(&evidence.sample_artifact, package::MAX_FILE)?;
        package::validate_sample_png(&artifact)?;
        let format = "png";
        let sample_sha256 = sha256(&artifact);
        let evidence_dir = self.root.join("evidence");
        fs::create_dir_all(&evidence_dir)?;
        package::reject_symlink(&evidence_dir)?;
        let evidence_path = evidence_dir.join(format!("{sample_sha256}.{format}"));
        if evidence_path.exists() {
            if package::read_bounded(&evidence_path, package::MAX_FILE)? != artifact {
                return Err(SkillError::Integrity("sample evidence hash".into()));
            }
        } else {
            package::write_new(&evidence_path, &artifact)?;
        }
        let mut next = self.index.clone();
        let entry = next.entries.get_mut(skill_id).ok_or_else(|| SkillError::NotFound(skill_id.into()))?;
        let revised = entry.revisions.iter_mut().find(|r| r.revision == revision).ok_or_else(|| SkillError::NotFound(revision.into()))?;
        revised.status = RevisionStatus::Approved;
        revised.approval =
            Some(ApprovalRecord { automatic, reviewer: evidence.reviewer, sample_sha256, sample_format: format.into(), notes: evidence.notes });
        entry.active_revision = Some(revision.into());
        entry.name = revised.name.clone();
        entry.description = revised.description.clone();
        self.persist(next)
    }

    pub fn set_enabled(&mut self, skill_id: &str, enabled: bool) -> Result<()> {
        let mut next = self.index.clone();
        next.entries.get_mut(skill_id).ok_or_else(|| SkillError::NotFound(skill_id.into()))?.enabled = enabled;
        self.persist(next)
    }

    pub fn rollback(&mut self, skill_id: &str, approved_revision: &str) -> Result<()> {
        let record = self.revision(skill_id, approved_revision)?;
        if record.status != RevisionStatus::Approved || record.approval.is_none() {
            return Err(SkillError::Invalid("rollback requires a previously approved revision".into()));
        }
        self.verified_files(record)?;
        let mut next = self.index.clone();
        let entry = next.entries.get_mut(skill_id).ok_or_else(|| SkillError::NotFound(skill_id.into()))?;
        entry.active_revision = Some(approved_revision.into());
        entry.name = record.name.clone();
        entry.description = record.description.clone();
        self.persist(next)
    }

    /// Select only needed textual references. Binary references remain cached
    /// source assets and are never implicitly appended to model requests.
    pub fn snapshot(&self, skill_id: &str, selected_references: &[String]) -> Result<SkillSnapshot> {
        let entry = self.index.entries.get(skill_id).ok_or_else(|| SkillError::NotFound(skill_id.into()))?;
        if !entry.enabled {
            return Err(SkillError::Unsupported("skill is disabled".into()));
        }
        let revision = entry.active_revision.as_ref().ok_or_else(|| SkillError::Unsupported("skill has no approved revision".into()))?;
        let record = self.revision(skill_id, revision)?;
        // Authenticate the index's file list without loading unselected source
        // PDFs or binary assets. Every actually loaded file is hash-checked.
        if package_hash(&record.file_hashes) != record.revision {
            return Err(SkillError::Integrity("revision file list changed".into()));
        }
        // Catalog metadata cannot inject a profile or change template permissions.
        if record.file_hashes.contains_key("designcraft.json") {
            let manifest: NativeManifest = serde_json::from_slice(&self.verified_file(record, "designcraft.json")?)?;
            if serde_json::to_value(&manifest)? != serde_json::to_value(&record.manifest)? {
                return Err(SkillError::Integrity("manifest metadata changed".into()));
            }
        }
        let skill_bytes = self.verified_file(record, "SKILL.md")?;
        let instructions = package::instructions(&skill_bytes)?;
        if instructions.len() > MAX_SNAPSHOT_TEXT_BYTES {
            return Err(SkillError::Invalid("skill snapshot instructions and references exceed the 64-KiB text context limit".into()));
        }
        let mut text_bytes = instructions.len();
        let mut references = BTreeMap::new();
        let mut reference_hashes = BTreeMap::new();
        for name in selected_references {
            if !record.manifest.references.contains(name) {
                return Err(SkillError::Invalid("reference is not declared by the skill".into()));
            }
            if !is_supported_text_reference(name) {
                return Err(SkillError::Unsupported("binary references require cached analysis".into()));
            }
            let remaining = MAX_SNAPSHOT_TEXT_BYTES.saturating_sub(text_bytes);
            let bytes = self.verified_file_bounded(record, name, remaining as u64).map_err(|error| match error {
                SkillError::Invalid(message) if message == "file type or size limit" || message == "file size limit" => {
                    SkillError::Invalid("skill snapshot instructions and references exceed the 64-KiB text context limit".into())
                }
                other => other,
            })?;
            text_bytes = text_bytes.checked_add(bytes.len()).ok_or_else(|| SkillError::Invalid("skill snapshot text size overflow".into()))?;
            let text = String::from_utf8(bytes.clone()).map_err(|_| SkillError::Invalid("reference is not UTF-8".into()))?;
            references.insert(name.clone(), text);
            reference_hashes.insert(name.clone(), sha256(&bytes));
        }
        let mut templates = BTreeMap::new();
        for name in &record.manifest.templates {
            templates.insert(name.clone(), package::parse_actions(&self.verified_file(record, name)?)?);
        }
        Ok(SkillSnapshot {
            skill_id: skill_id.into(),
            revision: revision.clone(),
            instructions,
            references,
            reference_hashes,
            templates,
            design_profile: record.manifest.design_profile.clone(),
        })
    }

    pub fn cache_analysis(&mut self, reference_sha256: &str, analyzer: &str, text: &str) -> Result<()> {
        validate_revision_id(reference_sha256)?;
        if analyzer.trim().is_empty() || analyzer.len() > 256 || text.len() > MAX_CACHE_TEXT {
            return Err(SkillError::Invalid("analysis cache limits".into()));
        }
        let key = sha256(format!("{reference_sha256}\n{analyzer}").as_bytes());
        let mut next = self.index.clone();
        next.analysis_cache.insert(key, CachedAnalysis { reference_sha256: reference_sha256.into(), analyzer: analyzer.into(), text: text.into() });
        self.persist(next)
    }

    pub fn cached_analysis(&self, reference_sha256: &str, analyzer: &str) -> Option<&CachedAnalysis> {
        self.index.analysis_cache.get(&sha256(format!("{reference_sha256}\n{analyzer}").as_bytes()))
    }

    pub fn export_folder(&self, skill_id: &str, revision: &str, destination: impl AsRef<Path>) -> Result<()> {
        let files = self.verified_files(self.revision(skill_id, revision)?)?;
        package::write_folder(destination.as_ref(), &files)
    }

    pub fn export_zip(&self, skill_id: &str, revision: &str, destination: impl AsRef<Path>) -> Result<()> {
        let files = self.verified_files(self.revision(skill_id, revision)?)?;
        package::write_zip(destination.as_ref(), &files)
    }

    pub fn install_builtin_drafts(&mut self) -> Result<Vec<SkillRevision>> {
        builtin::packages()?
            .into_iter()
            .map(|files| {
                let license = match files.get("references/LICENSE.txt") {
                    Some(bytes) if bytes.starts_with(b"MIT License") => "MIT",
                    Some(_) => "Apache-2.0",
                    None => "MIT OR Apache-2.0",
                };
                self.import_files(
                    files,
                    Provenance {
                        source: "DesignCraft native builtin; upstream attribution, where applicable, is in references/NOTICE.md".into(),
                        author: Some("DesignCraft contributors".into()),
                        license: Some(license.into()),
                        source_sha256: None,
                    },
                )
            })
            .collect()
    }

    fn revision_dir(&self, skill_id: &str, revision: &str) -> Result<PathBuf> {
        package::validate_id(skill_id)?;
        validate_revision_id(revision)?;
        let revisions = self.root.join("revisions");
        if revisions.exists() {
            package::reject_symlink(&revisions)?;
        }
        let skill_dir = revisions.join(skill_id);
        if skill_dir.exists() {
            package::reject_symlink(&skill_dir)?;
        }
        Ok(skill_dir.join(revision))
    }

    fn verified_files(&self, record: &SkillRevision) -> Result<PackageFiles> {
        let files = package::read_folder(&self.revision_dir(&record.skill_id, &record.revision)?)?;
        let hashes: BTreeMap<_, _> = files.iter().map(|(p, bytes)| (p.clone(), sha256(bytes))).collect();
        if hashes != record.file_hashes || package_hash(&hashes) != record.revision {
            return Err(SkillError::Integrity(format!("{}@{}", record.skill_id, record.revision)));
        }
        let parsed = package::parse(&files)?;
        // Catalog metadata is not trusted to grant capabilities over file content.
        if serde_json::to_value(&parsed.manifest)? != serde_json::to_value(&record.manifest)? {
            return Err(SkillError::Integrity("manifest metadata changed".into()));
        }
        Ok(files)
    }

    fn verified_file(&self, record: &SkillRevision, name: &str) -> Result<Vec<u8>> {
        self.verified_file_bounded(record, name, package::MAX_FILE)
    }

    fn verified_file_bounded(&self, record: &SkillRevision, name: &str, max_bytes: u64) -> Result<Vec<u8>> {
        package::validate_package_path(name)?;
        let expected = record.file_hashes.get(name).ok_or_else(|| SkillError::Integrity(name.into()))?;
        let root = self.revision_dir(&record.skill_id, &record.revision)?;
        package::reject_symlink(&root)?;
        let path = root.join(name);
        for ancestor in path.ancestors().skip(1).take_while(|p| *p != root) {
            package::reject_symlink(ancestor)?;
        }
        let bytes = package::read_bounded(&path, max_bytes.min(package::MAX_FILE))?;
        if sha256(&bytes) != *expected {
            return Err(SkillError::Integrity(name.into()));
        }
        Ok(bytes)
    }

    fn persist(&mut self, next: Index) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(&next)?;
        if bytes.len() as u64 > MAX_INDEX {
            return Err(SkillError::Invalid("catalog size limit".into()));
        }
        let pending = self.root.join("catalog.pending");
        package::reject_symlink(&self.root)?;
        if pending.exists() {
            package::reject_symlink(&pending)?;
            fs::remove_file(&pending)?;
        }
        package::write_new(&pending, &bytes)?;
        fs::rename(&pending, self.root.join("catalog.json"))?;
        self.index = next;
        Ok(())
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn package_hash(hashes: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (path, hash) in hashes {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(hash.as_bytes());
        hasher.update([0]);
    }
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn validate_revision_id(revision: &str) -> Result<()> {
    if revision.len() != 64 || !revision.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
        return Err(SkillError::Invalid("revision must be a lowercase SHA-256".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
