use super::{
    AiSettings, AiSnapshot, BTreeMap, BTreeSet, CredentialStatus, EngineAction, Instant, Path, PathBuf, PendingImageApproval, Prefs, ProductionDraft,
    ProductionRunner, Provenance, ProviderKind, SecretString, SessionKey, SkillRegistry, TaskReport, TaskRequest, UsageLedger, Vault,
    apply_execution_settings_to_draft, batch, file_access, read_json, validate_endpoint, validate_production_draft, validate_settings, worker,
};

pub(super) struct Pending {
    pub(super) roots: Option<std::sync::Arc<file_access::AutomationRoots>>,
    pub(super) worker: worker::Worker,
    pub(super) id: String,
    pub(super) started: Instant,
    pub(super) task: Option<TaskRequest>,
    pub(super) expected: Option<(u64, u64)>,
    pub(super) expected_doc: Option<std::sync::Arc<designcraft_engine::doc::Document>>,
    pub(super) expected_prefs: Option<Prefs>,
    pub(super) skills: Vec<(String, String)>,
    pub(super) settings: AiSettings,
    pub(super) deferred_outcome: Option<worker::Outcome>,
    pub(super) deferred_batch: Option<(worker::Outcome, Box<UsageLedger>)>,
}
pub(super) struct SampleEvidence {
    pub(super) id: String,
    pub(super) revision: String,
    pub(super) actions: Vec<EngineAction>,
    pub(super) path: PathBuf,
}
pub(super) struct PendingImageRegen {
    pub(super) approval: PendingImageApproval,
    pub(super) task: TaskRequest,
    pub(super) skills: Vec<(String, String)>,
    pub(super) style_profile: Option<designcraft_ai_types::DesignProfile>,
    pub(super) batches: Vec<crate::doc_tools::DeferredImagePlan>,
    pub(super) min_ppi: f64,
    pub(super) expected_doc: std::sync::Arc<designcraft_engine::doc::Document>,
}

impl ProductionRunner {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        // Pin the host's private profile independently of command automation roots.
        // Every profile access resolves links against these canonical boundaries.
        let profile_access = file_access::AutomationRoots::new(&[root.as_path()], &[root.as_path()])?.ok_or("Missing AI profile boundary")?;
        let vault = Vault::for_profile(&root)?;
        let mut registry = SkillRegistry::open(root.join("skills")).map_err(|e| e.to_string())?;
        registry.install_builtin_drafts().map_err(|e| e.to_string())?;
        let raw_settings = read_json(&profile_access, &root.join("settings.json"), 1024 * 1024)?;
        let legacy_settings_without_pacing = raw_settings.as_ref().is_some_and(|val| val.get("execution_pacing").is_none());
        let mut settings =
            raw_settings.map(serde_json::from_value::<AiSettings>).transpose().map_err(|e| format!("AI settings: {e}"))?.unwrap_or_default();
        if legacy_settings_without_pacing {
            settings.execution_pacing = designcraft_ai_types::ExecutionPacing::AutoContinue;
        }
        let mut startup_warning = String::new();
        if !settings.text_model.model.trim().is_empty() && designcraft_ai_types::is_non_text_model(&settings.text_model.model) {
            startup_warning = format!(
                "Cleared legacy non-text model '{}' from saved AI text settings; choose a text model for planning",
                settings.text_model.model.trim()
            );
            settings.text_model.model.clear();
        }
        validate_settings(&settings)?;
        let reports = read_json(&profile_access, &root.join("history.json"), 8 * 1024 * 1024)?
            .map(serde_json::from_value::<Vec<TaskReport>>)
            .transpose()
            .map_err(|e| format!("AI history: {e}"))?
            .unwrap_or_default();
        let print_attempts = read_json(&profile_access, &root.join("print-attempts.json"), 1024 * 1024)?
            .map(serde_json::from_value::<BTreeSet<String>>)
            .transpose()
            .map_err(|e| format!("Print attempt history: {e}"))?
            .unwrap_or_default();
        let recovered_batch = batch::recover_snapshot(&profile_access, &root)?;
        let settings_mtime = std::fs::metadata(root.join("settings.json")).and_then(|m| m.modified()).ok();
        let draft_mtime = std::fs::metadata(root.join("production-draft.json")).and_then(|m| m.modified()).ok();
        let (production_draft, mut draft_warning) = match read_json(&profile_access, &root.join("production-draft.json"), 2 * 1024 * 1024)
            .and_then(|value| value.map(serde_json::from_value::<ProductionDraft>).transpose().map_err(|e| e.to_string()))
            .and_then(|mut draft| {
                if let Some(draft) = &mut draft {
                    if !draft.request.settings.text_model.model.trim().is_empty()
                        && designcraft_ai_types::is_non_text_model(&draft.request.settings.text_model.model)
                    {
                        draft.request.settings.text_model = settings.text_model.clone();
                    }
                    validate_production_draft(draft)?;
                }
                Ok(draft)
            }) {
            Ok(mut draft) => {
                if let Some(draft) = &mut draft {
                    // UIDs are process-local. A saved path helps the human choose a book;
                    // neither it nor this draft authorizes publication or paid requests.
                    draft.document_uid = None;
                    if let (Some(s_time), Some(d_time)) = (settings_mtime, draft_mtime)
                        && s_time >= d_time
                    {
                        apply_execution_settings_to_draft(draft, &settings);
                    } else if designcraft_ai_types::is_non_image_model(
                        draft.request.settings.image_model.provider,
                        &draft.request.settings.image_model.model,
                    ) && (settings.image_model.provider == ProviderKind::ComfyUi
                        || (!settings.image_model.model.trim().is_empty()
                            && !designcraft_ai_types::is_non_image_model(settings.image_model.provider, &settings.image_model.model)))
                    {
                        draft.request.settings.image_model = settings.image_model.clone();
                    }
                }
                (draft, String::new())
            }
            Err(error) => (None, format!("Saved production instructions could not be restored: {error}")),
        };
        if !startup_warning.is_empty() {
            if draft_warning.is_empty() {
                draft_warning = startup_warning;
            } else {
                draft_warning = format!("{startup_warning}; {draft_warning}");
            }
        }
        let mut initial_stage = String::new();
        if draft_warning.is_empty()
            && recovered_batch.as_ref().is_some_and(|batch| batch.status == "failed")
            && let Some(last_failed) = reports.last().filter(|report| report.status == "failed" && !report.message.trim().is_empty())
        {
            draft_warning = last_failed.message.clone();
            initial_stage = "failed".into();
        }
        let mut host = Self {
            root,
            profile_access,
            registry,
            snapshot: AiSnapshot {
                available: true,
                stage: initial_stage,
                settings,
                reports,
                production_draft,
                message: draft_warning,
                batch: recovered_batch,
                ..Default::default()
            },
            keys: Vec::new(),
            vault,
            pending: None,
            pending_image_regen: None,
            batch: None,
            sample: None,
            sequence: 0,
            print_attempts,
            print_contracts: BTreeMap::new(),
            local_server: Default::default(),
        };
        if let Some(dir) = std::env::var_os("DESIGNCRAFT_SKILLS_DIR").filter(|val| !val.is_empty())
            && let Err(error) = host.install_portable_catalog(Path::new(&dir))
        {
            host.snapshot.skill_warnings.push(format!("Portable skills catalog warning: {error}"));
        }
        host.refresh_catalog();
        host.refresh_credentials();
        Ok(host)
    }

    /// Apply desktop-first defaults (`ExecutionPacing::AutoContinue`) when no saved `settings.json`
    /// existed yet in the user's profile, without altering programmatic test defaults.
    pub fn enable_desktop_defaults(&mut self) {
        if !self.root.join("settings.json").exists() {
            self.snapshot.settings.execution_pacing = designcraft_ai_types::ExecutionPacing::AutoContinue;
        }
    }

    /// Install verified portable skill packages from the first discovered `skills/catalog.json`
    /// directory (`DESIGNCRAFT_SKILLS_DIR`, adjacent to executable, or workspace `skills/` in debug
    /// builds). Never searches the untrusted current working directory. Returns `Ok(0)` when absent.
    pub fn install_default_portable_catalog(&mut self) -> Result<usize, String> {
        let mut candidates = Vec::new();
        if let Some(dir) = std::env::var_os("DESIGNCRAFT_SKILLS_DIR").filter(|val| !val.is_empty()) {
            candidates.push(PathBuf::from(dir));
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(parent) = exe.parent()
        {
            candidates.push(parent.join("skills"));
            if let Some(grandparent) = parent.parent() {
                candidates.push(grandparent.join("skills"));
                if let Some(workspace_root) = grandparent.parent() {
                    candidates.push(workspace_root.join("skills"));
                }
            }
        }
        #[cfg(debug_assertions)]
        candidates.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../skills"));
        for dir in candidates {
            let manifest = dir.join("catalog.json");
            if std::fs::symlink_metadata(&dir).is_ok_and(|m| !m.file_type().is_symlink() && m.is_dir())
                && std::fs::symlink_metadata(&manifest).is_ok_and(|m| !m.file_type().is_symlink() && m.is_file())
            {
                return self.install_portable_catalog(&dir);
            }
        }
        Ok(0)
    }

    /// Verify every listed file's SHA-256 digest and symlink-free path against `dir/catalog.json`
    /// before importing each package as a draft into the profile's `SkillRegistry`.
    pub fn install_portable_catalog(&mut self, dir: &Path) -> Result<usize, String> {
        let dir_meta = std::fs::symlink_metadata(dir).map_err(|e| format!("skills catalog directory: {e}"))?;
        if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
            return Err("Portable skills directory must be a non-symlink directory".into());
        }
        let catalog_bytes = read_catalog_file(&dir.join("catalog.json"), 2 * 1024 * 1024)?;
        let manifest_sha = hex_sha256(&catalog_bytes);
        let authentic_catalog = manifest_sha == hex_sha256(EMBEDDED_PORTABLE_CATALOG);
        let provenance_source = if authentic_catalog { "Smart Design portable catalog" } else { "External portable catalog" };
        let manifest: PortableCatalogManifest = serde_json::from_slice(&catalog_bytes).map_err(|e| format!("skills catalog.json: {e}"))?;
        if manifest.schema_version != 1 {
            return Err(format!("Unsupported portable skills catalog schema_version {}", manifest.schema_version));
        }
        if manifest.packages.len() > 256 {
            return Err("Portable skills catalog exceeds the 256-package limit".into());
        }
        let mut imported = 0usize;
        for pkg in manifest.packages {
            let pkg_id = pkg.id.clone();
            let outcome = (|| -> Result<(), String> {
                validate_portable_package_id(&pkg.id)?;
                if pkg.files.is_empty() || pkg.files.len() > 64 {
                    return Err("package must declare 1 to 64 files".into());
                }
                let pkg_dir = dir.join(&pkg.id);
                let pkg_meta = std::fs::symlink_metadata(&pkg_dir).map_err(|e| e.to_string())?;
                if pkg_meta.file_type().is_symlink() || !pkg_meta.is_dir() {
                    return Err("package path must be a non-symlink directory".into());
                }
                let mut files = designcraft_ai_skills::PackageFiles::new();
                let mut total_bytes = 0usize;
                for (rel_path, expected_sha) in &pkg.files {
                    if expected_sha.len() != 64 || !expected_sha.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
                        return Err(format!("invalid SHA-256 digest for {rel_path}"));
                    }
                    let full_path = resolve_portable_package_file(&pkg_dir, rel_path)?;
                    let bytes = read_catalog_file(&full_path, 512 * 1024)?;
                    total_bytes = total_bytes
                        .checked_add(bytes.len())
                        .filter(|sum| *sum <= 4 * 1024 * 1024)
                        .ok_or_else(|| "package exceeds 4-MiB total size limit".to_string())?;
                    let actual_sha = hex_sha256(&bytes);
                    if actual_sha != *expected_sha {
                        return Err(format!("SHA-256 mismatch for {rel_path}"));
                    }
                    files.insert(rel_path.clone(), bytes);
                }
                self.registry
                    .import_files(
                        files,
                        Provenance { source: provenance_source.into(), author: None, license: None, source_sha256: Some(manifest_sha.clone()) },
                    )
                    .map_err(|e| e.to_string())?;
                Ok(())
            })();
            match outcome {
                Ok(()) => imported = imported.saturating_add(1),
                Err(error) => {
                    if self.snapshot.skill_warnings.len() < 64 {
                        self.snapshot.skill_warnings.push(format!(
                            "Skipped portable skill {}: {}",
                            pkg_id.chars().take(64).collect::<String>(),
                            error.chars().take(512).collect::<String>()
                        ));
                    }
                }
            }
        }
        self.refresh_catalog();
        Ok(imported)
    }

    pub(super) fn configured_key_selection(&self, provider: ProviderKind) -> Result<designcraft_ai_types::ModelSelection, String> {
        let selections = selected_models(&self.snapshot.settings);
        let mut matching = selections.into_iter().filter(|selection| selection.provider == provider);
        let selected = matching.next().ok_or("Select this provider in the current AI settings before setting its session key")?;
        if matching.any(|other| validate_endpoint(&other).ok() != validate_endpoint(&selected).ok()) {
            return Err("This provider has multiple endpoints; save its key using the endpoint-specific key field".into());
        }
        Ok(selected)
    }

    pub(super) fn store_session_key(&mut self, selection: &designcraft_ai_types::ModelSelection, secret: SecretString) -> Result<(), String> {
        let stored = SessionKey::new(selection, secret)?;
        self.keys.retain(|key| !key.matches(selection));
        if self.keys.len() >= 32 {
            return Err("Too many session credential endpoints; restart the application to clear unused session keys".into());
        }
        self.keys.push(stored);
        Ok(())
    }

    pub(super) fn worker_keys(&self) -> Vec<SessionKey> {
        self.keys.iter().map(SessionKey::duplicate).collect()
    }

    pub(super) fn prepare_credentials(&mut self, selections: &[designcraft_ai_types::ModelSelection]) -> Result<(), String> {
        for selection in selections {
            validate_endpoint(selection)?;
            if self.keys.iter().any(|key| key.matches(selection)) {
                continue;
            }
            if !Vault::available() {
                continue;
            }
            let stored = credential_from_store_or_transient(
                selection,
                || self.vault.load(selection),
                || crate::vault::selected_key(&self.keys, selection).is_some(),
            )?;
            if let Some(secret) = stored {
                self.store_session_key(selection, secret)?;
            }
        }
        Ok(())
    }

    pub(super) fn refresh_credentials(&mut self) {
        let selections = selected_models(&self.snapshot.settings);
        self.snapshot.credentials = selections
            .into_iter()
            .map(|selection| {
                let presence = if Vault::available() {
                    self.vault.presence(&selection)
                } else {
                    Err("OS-protected credential storage is unavailable; session keys remain in memory only".into())
                };
                let transient_available = crate::vault::selected_key(&self.keys, &selection).is_some();
                credential_status(selection, presence, transient_available)
            })
            .collect();
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PortableCatalogManifest {
    pub(super) schema_version: u32,
    pub(super) packages: Vec<PortableCatalogPackage>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PortableCatalogPackage {
    pub(super) id: String,
    pub(super) files: BTreeMap<String, String>,
}

pub(super) const EMBEDDED_PORTABLE_CATALOG: &[u8] = include_bytes!("../../../../skills/catalog.json");

pub(super) fn is_trusted_catalog_provenance(provenance: &Provenance) -> bool {
    provenance.source.starts_with("DesignCraft native builtin")
        || (provenance.source == "Smart Design portable catalog"
            && provenance.source_sha256.as_deref() == Some(hex_sha256(EMBEDDED_PORTABLE_CATALOG).as_str()))
}

pub(super) fn validate_portable_package_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_')) {
        return Err("invalid portable package identifier".into());
    }
    let mut comps = Path::new(id).components();
    if !matches!(comps.next(), Some(std::path::Component::Normal(_))) || comps.next().is_some() {
        return Err("portable package identifier must be a single path segment".into());
    }
    Ok(())
}

pub(super) fn resolve_portable_package_file(pkg_dir: &Path, rel_path: &str) -> Result<PathBuf, String> {
    if rel_path.is_empty() || rel_path.len() > 256 || rel_path.contains('\\') {
        return Err(format!("invalid portable package file path: {rel_path}"));
    }
    let mut current = pkg_dir.to_path_buf();
    let segments: Vec<_> = Path::new(rel_path).components().collect();
    if segments.is_empty() || segments.len() > 8 {
        return Err(format!("portable package file depth out of bounds: {rel_path}"));
    }
    for (idx, comp) in segments.iter().enumerate() {
        let std::path::Component::Normal(seg) = comp else {
            return Err(format!("non-normal portable package file path: {rel_path}"));
        };
        current.push(seg);
        let meta = std::fs::symlink_metadata(&current).map_err(|e| format!("{rel_path}: {e}"))?;
        if meta.file_type().is_symlink() {
            return Err(format!("symlink forbidden in portable package path: {rel_path}"));
        }
        let is_last = idx + 1 == segments.len();
        if is_last && !meta.is_file() {
            return Err(format!("portable package entry is not a regular file: {rel_path}"));
        }
        if !is_last && !meta.is_dir() {
            return Err(format!("portable package intermediate segment is not a directory: {rel_path}"));
        }
    }
    Ok(current)
}

pub(super) fn read_catalog_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err("catalog entry must be a regular non-symlink file".into());
    }
    if meta.len() > max_bytes {
        return Err("catalog entry exceeds byte limit".into());
    }
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let limit = max_bytes.saturating_add(1);
    let mut buf = Vec::with_capacity(usize::try_from(meta.len().min(64 * 1024)).unwrap_or(0));
    file.take(limit).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if u64::try_from(buf.len()).map_or(true, |len| len > max_bytes) {
        return Err("catalog entry grew past byte limit".into());
    }
    Ok(buf)
}

pub(super) fn hex_sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let hi = char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0');
        let lo = char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0');
        out.push(hi);
        out.push(lo);
    }
    out
}

pub(super) fn selected_models(settings: &AiSettings) -> Vec<designcraft_ai_types::ModelSelection> {
    let text = settings.text_model.clone();
    let image = settings.image_model.clone();
    if text.provider == image.provider && validate_endpoint(&text).ok() == validate_endpoint(&image).ok() { vec![text] } else { vec![text, image] }
}

/// Stored credentials keep precedence over environment fallbacks. A storage
/// error may be ignored only when the caller's endpoint-aware resolver confirms
/// a usable transient key. This helper never serializes or returns that key.
pub(super) fn credential_from_store_or_transient(
    selection: &designcraft_ai_types::ModelSelection,
    load: impl FnOnce() -> Result<Option<SecretString>, String>,
    transient_available: impl FnOnce() -> bool,
) -> Result<Option<SecretString>, String> {
    validate_endpoint(selection)?;
    match load() {
        Ok(stored) => Ok(stored),
        Err(error) => {
            if transient_available() {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

pub(super) fn credential_status(
    selection: designcraft_ai_types::ModelSelection,
    presence: Result<bool, String>,
    transient_available: bool,
) -> CredentialStatus {
    let protected_storage_available = presence.is_ok();
    let persisted = presence.as_ref().copied().unwrap_or(false);
    let available = persisted || transient_available;
    let message = match presence {
        Ok(true) => "OS-protected key stored for this endpoint".into(),
        Ok(false) if transient_available => {
            "Credential available from an authorized session or canonical provider environment; no protected key stored".into()
        }
        Ok(false) => "No credential available for this endpoint; no protected key stored".into(),
        Err(error) if transient_available => format!("Credential available from an authorized session or canonical provider environment; {error}"),
        Err(error) => format!("No credential available for this endpoint; {error}"),
    };
    CredentialStatus { selection, persisted, available, protected_storage_available, message }
}
