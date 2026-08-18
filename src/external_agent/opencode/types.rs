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
    pub native: bool,
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
/// Strips the `mode` and `native` fields, projecting an [`Agent`] down to
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

// ─── Session messages (GET /session/{id}/message) ──────────────────

/// A message entry returned by `GET /session/{id}/message`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionMessage {
    pub info: SessionMessageInfo,
    #[serde(default)]
    pub parts: Vec<SessionMessagePart>,
}

impl SessionMessage {
    pub fn is_user(&self) -> bool {
        self.info.role == "user"
    }

    pub fn text(&self) -> String {
        let mut result = String::new();
        for part in &self.parts {
            if let SessionMessagePart::Text { text } = part
                && !text.is_empty()
            {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(text);
            }
        }
        result
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SessionMessageInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub role: String,
    #[serde(default, rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub time: Option<SessionTime>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SessionMessagePart {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// A workspace returned by `POST /experimental/workspace`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Workspace {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub directory: Option<String>,
    #[serde(default)]
    pub extra: Option<serde_json::Value>,
    #[serde(rename = "projectID")]
    pub project_id: String,
    #[serde(default)]
    pub time_used: Option<serde_json::Value>,
}

/// A git worktree returned by `POST /experimental/worktree`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Worktree {
    pub name: String,
    #[serde(default)]
    pub branch: Option<String>,
    pub directory: String,
}
