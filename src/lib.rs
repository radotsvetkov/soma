//! soma — transparent, policy-governed, self-improving agent runtime.
//! Zero external dependencies; see SPEC.md.

pub mod aiact;
pub mod anchor;
pub mod attest;
pub mod cache;
pub mod cli;
pub mod cron;
pub mod events;
pub mod export;
pub mod goals;
pub mod improve;
pub mod http;
pub mod json;
pub mod knowledge;
pub mod mcp;
pub mod models;
pub mod neuro;
pub mod skills;
pub mod policy;
pub mod project;
pub mod sha256;
pub mod util;
pub mod wrap;

/// CLI entrypoint; returns the process exit code.
pub fn run() -> i32 {
    cli::run()
}
