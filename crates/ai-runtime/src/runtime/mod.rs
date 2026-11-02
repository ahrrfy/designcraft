use crate::vault::{SessionKey, Vault};
use crate::{ledger::UsageLedger, science, worker};
use designcraft_ai_providers::{SecretString, validate_endpoint};
use designcraft_ai_skills::{ApprovalEvidence, Provenance, RevisionStatus, SkillRegistry, SkillSnapshot};
use designcraft_ai_types::protocol::{
    AiRequest, AiSettings, AiSnapshot, BatchReport, BatchSnapshot, CredentialStatus, ExecutionPacing, LocalServerStatus, MAX_BRIEF_BYTES,
    PendingImageApproval, PrintProof, ProductionDraft, ProductionPlan, SkillCard, SkillReview, TaskReport, TaskRequest, TaskTarget, UsageSummary,
    VisualDensityMode,
};
use designcraft_ai_types::{EngineAction, ProviderKind};
use designcraft_engine::{DocState, Prefs, Session, file_access};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{atomic::Ordering, mpsc::TryRecvError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_REPORTS: usize = 100;
#[path = "../batch.rs"]
mod batch;
mod dispatch;
#[path = "../output.rs"]
mod output;
mod persistence;
mod proofs;
mod state;
#[cfg(test)]
mod tests;

pub(crate) use self::persistence::{
    allocate_knowledge_pack_id, enforce_execution_mode, list_knowledge_pack_dir_ids, persist_knowledge_pack_files, remove_knowledge_pack_dir,
    require_generation_models, require_text_model,
};
#[allow(unused_imports)]
use self::persistence::{apply_execution_settings_to_draft, read_json, validate_production_draft, validate_settings, write_json, write_profile};
pub(crate) use self::proofs::book_binding;
#[cfg(test)]
#[allow(unused_imports)]
use self::proofs::new_a4;
#[allow(unused_imports)]
use self::proofs::{
    active_design_revision, bounded_task_skills, choose_compatible_skills, compact_context, corrected_design_context, design_revision,
    enrich_production_context, enrich_production_context_with_memory, ensure_preflight, ensure_preflight_at, ensure_publication_idle,
    initialize_facing_document, initialize_print_document, is_publication_temporarily_busy, merge_pinned_and_automatic_skills,
    normalize_skill_search, print_params, production_document_params, prompt_names_skill_id, selected_skill_references, skill_is_compatible,
    skill_match_score, supported_commands, task_document_context, task_skill_context_bytes, validate_skill_templates,
};
#[allow(unused_imports)]
use self::state::{
    EMBEDDED_PORTABLE_CATALOG, Pending, PendingImageRegen, PortableCatalogManifest, PortableCatalogPackage, SampleEvidence,
    credential_from_store_or_transient, credential_status, hex_sha256, is_trusted_catalog_provenance, read_catalog_file,
    resolve_portable_package_file, selected_models, validate_portable_package_id,
};

/// Native service injected by the desktop host. `handle` never waits on a provider.
pub struct ProductionRunner {
    root: PathBuf,
    profile_access: std::sync::Arc<file_access::AutomationRoots>,
    registry: SkillRegistry,
    snapshot: AiSnapshot,
    keys: Vec<SessionKey>,
    vault: Vault,
    pending: Option<Pending>,
    pending_image_regen: Option<PendingImageRegen>,
    batch: Option<batch::BatchJob>,
    sample: Option<SampleEvidence>,
    sequence: u64,
    print_attempts: BTreeSet<String>,
    print_contracts: BTreeMap<u64, crate::print_contract::PrintContract>,
    local_server: crate::localserver::LocalOllama,
    design_memory: BTreeMap<u64, crate::brief_planner::SessionDesignMemory>,
    failed_cloud_task: Option<(TaskRequest, Option<u64>)>,
}

impl Drop for ProductionRunner {
    fn drop(&mut self) {
        if let Some(job) = &self.pending {
            job.worker.cancel.store(true, Ordering::Relaxed);
        }
    }
}
