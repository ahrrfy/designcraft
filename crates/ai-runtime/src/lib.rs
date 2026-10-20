//! Native AI host: provider work runs off the UI thread; all design edits use engine commands.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod design;
pub mod indesign;
pub mod ledger;
pub mod localserver;
mod references;
pub mod resources;
mod runtime;
pub mod science;
mod template_plan;
pub mod vault;
mod worker;
pub use runtime::ProductionRunner;
