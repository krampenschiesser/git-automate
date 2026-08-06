//! Provider-agnostic abstractions for external AI agent APIs (e.g. OpenCode).
//!
//! Defines the [`ExternalAgent`] trait — a provider-agnostic interface for
//! managing AI agent sessions — along with the canonical error type and
//! session-info shape. Each provider implementation (e.g. OpenCode) maps the
//! trait's canonical operations to its underlying HTTP API calls.
//!
//! The concrete OpenCode implementation lives in [`crate::external_agent::opencode`].

use thiserror::Error;

// ─── Error type ────────────────────────────────────────────────

/// Errors that can occur while operating an [`ExternalAgent`].
#[derive(Debug, Error)]
pub enum ExternalAgentError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP status {0}")]
    HttpStatus(u16),
    #[error("Failed to parse response: {0}")]
    Parse(String),
    #[error("Session not found: {0}")]
    SessionNotFound(String),
    #[error("Failed to start session: {0}")]
    StartSession(String),
    #[error("Failed to execute shell command: {0}")]
    ShellCommand(String),
    #[error("Failed to get session status: {0}")]
    SessionStatus(String),
    #[error("Failed to nudge session: {0}")]
    Nudge(String),
    #[error("Failed to list sessions: {0}")]
    ListSessions(String),
    #[error("Failed to get session: {0}")]
    GetSession(String),
}

// ─── Canonical types ───────────────────────────────────────────

/// Status of an agent session from the daemon's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSessionStatus {
    /// Session has completed its work (or was removed).
    Done,
    /// Session is actively running or awaiting retry.
    Waiting,
    /// Session is idle and available.
    Idle,
}

/// Canonical session representation returned by any [`ExternalAgent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Unique session identifier.
    pub id: String,
    /// Human-readable session title.
    pub title: String,
    /// Working directory for the session.
    pub directory: String,
    /// Project ID the session belongs to.
    pub project_id: String,
}

// ─── The trait ─────────────────────────────────────────────────

/// Abstraction over any external AI agent server (OpenCode, Claude-Code, etc.).
///
/// Implementations translate the provider-agnostic operations below into the
/// underlying provider's HTTP API calls. The trait is `Send + Sync` so it can
/// be held behind an `Arc` in async workflow code.
pub trait ExternalAgent: Send + Sync {
    /// Start a session for *project_key* with the given system and user prompts.
    ///
    /// *project_key* is typically a working-directory path or project identifier.
    /// Returns the new session's ID.
    fn start_session(
        &self,
        project_key: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> impl std::future::Future<Output = Result<String, ExternalAgentError>> + Send;

    /// Execute *command* (e.g. `git commit`, `cargo test`) inside an existing session.
    fn execute_shell(
        &self,
        session_id: &str,
        command: &str,
    ) -> impl std::future::Future<Output = Result<String, ExternalAgentError>> + Send;

    /// Check whether a session is [`AgentSessionStatus::Done`],
    /// [`AgentSessionStatus::Waiting`], or [`AgentSessionStatus::Idle`].
    fn session_status(
        &self,
        session_id: &str,
    ) -> impl std::future::Future<Output = Result<AgentSessionStatus, ExternalAgentError>> + Send;

    /// Nudge a session to continue processing (e.g. wake a waiting/idle session).
    fn nudge_session(
        &self,
        session_id: &str,
    ) -> impl std::future::Future<Output = Result<(), ExternalAgentError>> + Send;

    /// Retrieve a session by ID. Returns `None` if the session does not exist.
    fn get_session(
        &self,
        session_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<SessionInfo>, ExternalAgentError>> + Send;

    /// List all sessions known to the agent server.
    fn list_sessions(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<SessionInfo>, ExternalAgentError>> + Send;
}
