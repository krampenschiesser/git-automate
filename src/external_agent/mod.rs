//! External agent module — provider-agnostic abstractions + OpenCode implementation.
//!
//! The `common` submodule defines the [`ExternalAgent`] trait, its error type,
//! and canonical session-info shape — everything provider-agnostic. The
//! `opencode` submodule provides the concrete `OpenCodeClient` implementation.

pub mod common;
pub mod opencode;

// Convenience re-exports for the provider-agnostic abstractions.
pub use common::{AgentSessionStatus, ExternalAgent, ExternalAgentError, SessionInfo};
