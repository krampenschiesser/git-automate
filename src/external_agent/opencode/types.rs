use serde::Deserialize;

/// Agent mode as reported by the OpenCode `/agent` endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Subagent,
    Primary,
    All,
    #[serde(other)]
    Other,
}

/// An agent descriptor returned by the OpenCode `/agent` endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct Agent {
    pub name: String,
    pub description: Option<String>,
    pub mode: AgentMode,
    #[serde(rename = "builtIn")]
    pub built_in: bool,
}

/// Timestamps associated with a session.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionTime {
    pub created: u64,
    pub updated: u64,
}

/// A session created via the OpenCode `/session` endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Session {
    pub id: String,
    #[serde(rename = "projectID")]
    pub project_id: String,
    pub directory: String,
    pub title: String,
    pub version: String,
    pub time: SessionTime,
}

/// Health-check response from the OpenCode `/global/health` endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct HealthResponse {
    pub healthy: bool,
    pub version: String,
}

/// Non-serde info struct returned by `get_agents`.
///
/// Strips the `mode` and `built_in` fields, projecting an [`Agent`] down to
/// just the user-facing `name` and `description`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentInfo {
    pub name: String,
    pub description: Option<String>,
}

impl From<Agent> for AgentInfo {
    fn from(agent: Agent) -> Self {
        AgentInfo {
            name: agent.name,
            description: agent.description,
        }
    }
}
