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

// ─── Session V2 listing (GET /api/session) ─────────────────────────

/// A model reference as returned by the OpenCode `/api/session` endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub id: String,
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(default)]
    pub variant: Option<String>,
}

impl ModelRef {
    /// Format as `"providerID/id"` matching the config key format.
    pub fn as_key(&self) -> String {
        format!("{}/{}", self.provider_id, self.id)
    }
}

/// A minimal session entry from `GET /api/session` — only the fields we need.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionV2Info {
    pub id: String,
    pub model: Option<ModelRef>,
}

/// Response wrapper for `GET /api/session`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionsResponse {
    pub data: Vec<SessionV2Info>,
    #[serde(default)]
    pub cursor: Option<Cursor>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Cursor {
    pub previous: Option<String>,
    pub next: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ref_as_key() {
        let model = ModelRef {
            id: "cuda-small/fast".into(),
            provider_id: "myprovider".into(),
            variant: None,
        };
        assert_eq!(model.as_key(), "myprovider/cuda-small/fast");
    }

    #[test]
    fn model_ref_as_key_with_variant() {
        let model = ModelRef {
            id: "claude-sonnet-4-20250514".into(),
            provider_id: "anthropic".into(),
            variant: Some("20250514".into()),
        };
        assert_eq!(model.as_key(), "anthropic/claude-sonnet-4-20250514");
    }

    #[test]
    fn session_v2_info_deserializes() {
        let json = serde_json::json!({
            "id": "ses_abc123",
            "model": {
                "id": "cuda-small/fast",
                "providerID": "myprovider"
            }
        });
        let info: SessionV2Info = serde_json::from_value(json).unwrap();
        assert_eq!(info.id, "ses_abc123");
        assert!(info.model.is_some());
        let model = info.model.unwrap();
        assert_eq!(model.id, "cuda-small/fast");
        assert_eq!(model.provider_id, "myprovider");
    }

    #[test]
    fn session_v2_info_missing_model() {
        let json = serde_json::json!({
            "id": "ses_xyz789"
        });
        let info: SessionV2Info = serde_json::from_value(json).unwrap();
        assert_eq!(info.id, "ses_xyz789");
        assert!(info.model.is_none());
    }

    #[test]
    fn sessions_response_deserializes() {
        let json = serde_json::json!({
            "data": [
                { "id": "ses_1", "model": { "id": "m1", "providerID": "p" } },
                { "id": "ses_2" }
            ],
            "cursor": { "previous": "prev", "next": "next" }
        });
        let resp: SessionsResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.data.len(), 2);
        assert_eq!(resp.data[0].id, "ses_1");
        assert_eq!(resp.data[1].id, "ses_2");
        assert!(resp.cursor.is_some());
        let cursor = resp.cursor.unwrap();
        assert_eq!(cursor.previous, Some("prev".into()));
        assert_eq!(cursor.next, Some("next".into()));
    }

    #[test]
    fn sessions_response_no_cursor() {
        let json = serde_json::json!({
            "data": []
        });
        let resp: SessionsResponse = serde_json::from_value(json).unwrap();
        assert!(resp.data.is_empty());
        assert!(resp.cursor.is_none());
    }
}
