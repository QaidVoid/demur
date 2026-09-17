//! Review pipeline core for demur, a BYOK code review bot for GitHub.
//!
//! The crate hosts everything distribution-independent: configuration
//! parsing, provider clients, diff ingestion, cost estimation, the
//! multi-pass review pipeline, and delta review state. The CLI and the
//! GitHub Action binaries are thin wrappers over this library.
