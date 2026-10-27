//! Native AI host: provider work runs off the UI thread; all design edits use engine commands.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod agent_loop;
mod design;
mod doc_tools;
mod facing_pages;
mod image_repetition;
mod image_review;
pub mod indesign;
pub mod ledger;
pub mod localserver;
mod native_layout;
mod native_print_fonts;
mod print_contract;
mod production_cache;
mod references;
pub mod resources;
mod runtime;
pub mod science;
mod script_contract;
mod template_plan;
pub mod vault;
mod worker;
pub use resources::{clear_active_gpu_adapter, set_active_gpu_adapter};
pub use runtime::ProductionRunner;
