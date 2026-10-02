//! OpenCode v2 HTTP client.
//!
//! Provides [`OpenCodeClient`], serde response types, and the endpoint methods
//! for the OpenCode v2 server API (all routes under `/api/*`).

pub mod client;
pub mod types;

// Convenience re-exports
pub use client::{Delivery, OpenCodeClient, OpenCodeError, encode_basic_auth};
pub use types::{
    ActiveSessionEntry, Agent, AgentMode, AssistantContent, Cursor, Data, LocationInfo,
    LocationProject, LocationRef, ModelRef, PromptReceipt, ServerInfo, ServerInfoPaths, Session,
    SessionMessage, SessionMessagesResponse, SessionOutcome, SessionTime, WorktreeInfo,
};
