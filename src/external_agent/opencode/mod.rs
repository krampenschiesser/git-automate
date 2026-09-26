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
    ActiveSessionEntry, Agent, AgentInfo, AgentV2, AssistantContent, Cursor, Data, HealthResponse,
    LocationInfo, LocationProject, LocationRef, ModelRef, PromptReceipt, ServerInfo,
    ServerInfoPaths, Session, SessionMessage, SessionMessageInfo, SessionMessageV2,
    SessionMessagesResponse, SessionOutcome, SessionTime, SessionTimeV2, SessionV2Info,
    SessionsResponse, Workspace, Worktree, WorktreeInfo,
};
