//! Review pipeline core for demur, a BYOK code review bot for GitHub.
//!
//! The crate hosts everything distribution-independent: configuration
//! parsing, provider clients, diff ingestion, cost estimation, the
//! multi-pass review pipeline, and delta review state. The CLI and the
//! GitHub Action binaries are thin wrappers over this library.

pub mod app;
pub mod cache;
pub mod config;
#[cfg(test)]
mod config_docs;
pub mod cost;
pub mod delta;
pub mod diff;
pub mod github;
pub mod ingest;
pub mod pipeline;
pub mod provider;
pub mod retrieval;
pub mod rules;
mod schema_check;
