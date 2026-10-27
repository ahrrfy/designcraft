//! Restricted, native AI document production. Plans execute on a private draft; only a complete
//! successful draft is published. Neither provider output nor a caller's allowlist grants IO.

mod apply;
mod image_guard;
mod layout_guard;
mod schema;
#[cfg(test)]
mod tests;

use serde_json::{Value, json};

use super::{CommandSpec, always, cmd, has_doc};

pub use apply::{apply_action_groups, apply_actions, validate_actions};
pub use schema::command_schema;

pub(super) const ID: &str = "production.apply";
pub(super) const GROUP_ID: &str = "production.applyGroup";
pub(super) const MAX_ACTIONS: usize = 512;
pub(super) const MAX_ALLOWED_COMMANDS: usize = 512;
pub(super) const MAX_BYTES: usize = 48 * 1024 * 1024;
pub(super) const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub(super) const MAX_ENCODED_IMAGE_BYTES: usize = MAX_IMAGE_BYTES.div_ceil(3) * 4;
pub(super) const MAX_IMAGE_PIXELS: u64 = 20_000_000;
pub(super) const MAX_UNIQUE_IMAGE_BYTES: usize = 32 * 1024 * 1024;
pub(super) const MAX_UNIQUE_IMAGE_PIXELS: u64 = 40_000_000;
pub(super) const MAX_GROUPS: usize = 1000;
pub(super) const MAX_GROUP_BYTES: usize = 512 * 1024 * 1024;

/// Catalog consumed by prompt builders and the AI permissions UI.
pub fn catalog() -> Value {
    let commands: Vec<_> = schema::schemas()
        .into_iter()
        .map(|(id, params_schema, example)| {
            json!({
                "command": id,
                "paramsSchema": params_schema,
                "example": example,
                "description": super::find_command(id).map(|s| s.params),
                "requiresImages": id == "file.place",
            })
        })
        .collect();
    json!({
        "version": 1,
        "outputModes": ["native_only", "native_with_images"],
        "maxActions": MAX_ACTIONS,
        "references": "$N.path and ${N.path}; zero-based earlier results, resolved values are revalidated",
        "commands": commands,
    })
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "production.catalog","AI Production Commands",[],None,"{} → vetted commands with paramsSchema and examples",always,|_,_| Ok(catalog())),
        cmd!(query "production.validate","Validate AI Production",[],None,"{commands: [{command, params}], outputMode, allowedCommands?, label?, expectedDocumentUid?, expectedRevision?, requirePreflight?: bool (default false; refuse error-level issues before publication), minPpi?: 150 (1 to 2400)} → {valid, steps, results}",has_doc,|s,p| validate_actions(s,p)),
        cmd!(noundo "production.apply","Apply AI Production",[],None,"{commands: [{command, params}], outputMode, allowedCommands?, label?, expectedDocumentUid?, expectedRevision?, requirePreflight?: bool (default false; refuse error-level issues before publication), minPpi?: 150 (1 to 2400)} → {applied, steps, results}",has_doc,|s,p| apply_actions(s,p,|| false)),
        cmd!(noundo "production.applyGroup","Apply AI Production Group",[],None,"{chunks: [{commands: [{command, params}], label?}] (1 to 1000 chunks, at most 512 commands each; references reset per chunk), outputMode, label?, expectedDocumentUid?, expectedRevision?, requirePreflight?: bool, minPpi?: 150 (1 to 2400), documentSetup?: {facingPages?: bool, binding?: leftToRight|rightToLeft} (host only; applied privately before chunks without changing page count)} → {applied, groups, steps, results: [chunk results], label, documentUid, revision}",has_doc,|s,p| apply_action_groups(s,p,|| false)),
    ]
}
