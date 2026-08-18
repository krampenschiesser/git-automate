//! OpenCode-specific implementation of the [`ExternalAgent`] trait.
//!
//! Provides the `OpenCodeClient` HTTP client, serde response types, and the
//! `impl ExternalAgent for OpenCodeClient` block. The provider-agnostic trait
//! and canonical types live in [`crate::external_agent::common`].

pub mod agent;
pub mod client;
pub mod types;

// Convenience re-exports
pub use client::{OpenCodeClient, OpenCodeError, encode_basic_auth};
pub use types::{
    Agent, AgentInfo, Cursor, HealthResponse, ModelRef, Session, SessionMessage,
    SessionMessageInfo, SessionTime, SessionV2Info, SessionsResponse, Workspace, Worktree,
};
