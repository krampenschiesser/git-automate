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
    #[serde(rename = "parentID")]
    pub parent_id: Option<String>,
    pub agent: Option<String>,
    pub outcome: Option<SessionOutcome>,
    #[serde(default)]
    pub time: Option<SessionTimeV2>,
    #[serde(default)]
    pub location: Option<LocationRef>,
    #[serde(rename = "projectID")]
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
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

// ─── v2 response types ─────────────────────────────────────────────

/// Generic `{ "data": ... }` response envelope.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Data<T> {
    pub data: T,
}

/// `GET /api/info` server information.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ServerInfo {
    pub version: String,
    pub pid: i64,
    pub urls: Vec<String>,
    pub paths: ServerInfoPaths,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ServerInfoPaths {
    pub tmp: String,
}

/// `GET /api/location` working directory and project.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LocationInfo {
    pub directory: String,
    pub project: LocationProject,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LocationProject {
    pub id: String,
    pub directory: String,
    pub canonical: String,
}

/// Reference to a working directory (session location / create-session body).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct LocationRef {
    pub directory: String,
}

/// A worktree returned by `POST /api/worktree`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct WorktreeInfo {
    pub directory: String,
}

/// Final outcome of a session.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum SessionOutcome {
    Succeeded,
    Failed,
    Interrupted,
}

/// Timestamps associated with a v2 session. JSON numbers may be int or float.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SessionTimeV2 {
    pub created: Option<f64>,
    pub updated: Option<f64>,
    pub idle: Option<f64>,
    pub viewed: Option<f64>,
    pub archived: Option<f64>,
}

/// Value of a `GET /api/session/active` map entry.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct ActiveSessionEntry {
    #[serde(rename = "type")]
    pub kind: String,
}

/// Admission receipt returned when prompting a session.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct PromptReceipt {
    #[serde(rename = "id")]
    pub id: String,
}

/// An agent descriptor from the v2 `GET /api/agent` endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct AgentV2 {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub mode: Option<AgentMode>,
    #[serde(default)]
    pub model: Option<ModelRef>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub hidden: Option<bool>,
}

/// A message entry from the v2 `GET /api/session/{id}/message` endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SessionMessageV2 {
    User {
        id: String,
        #[serde(default)]
        time: Option<SessionTimeV2>,
        #[serde(default)]
        text: String,
        #[serde(default)]
        delivery: Option<String>,
    },
    Assistant {
        id: String,
        #[serde(default)]
        time: Option<SessionTimeV2>,
        #[serde(default)]
        content: Vec<AssistantContent>,
    },
    #[serde(other)]
    Other,
}

impl SessionMessageV2 {
    pub fn text(&self) -> String {
        match self {
            SessionMessageV2::User { text, .. } => text.clone(),
            SessionMessageV2::Assistant { content, .. } => {
                let mut result = String::new();
                for entry in content {
                    if let AssistantContent::Text { text } = entry {
                        if !result.is_empty() {
                            result.push('\n');
                        }
                        result.push_str(text);
                    }
                }
                result
            }
            SessionMessageV2::Other => String::new(),
        }
    }

    pub fn is_user(&self) -> bool {
        matches!(self, SessionMessageV2::User { .. })
    }
}

/// A content entry within an assistant v2 message.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum AssistantContent {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

/// Response wrapper for the v2 `GET /api/session/{id}/message` endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionMessagesResponse {
    pub data: Vec<SessionMessageV2>,
    #[serde(default)]
    pub cursor: Option<Cursor>,
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

    // ─── v2 response types ─────────────────────────────────────────

    #[test]
    fn data_envelope_unwraps() {
        let json = serde_json::json!({ "data": { "id": "ses_1" } });
        let envelope: Data<SessionV2Info> = serde_json::from_value(json).unwrap();
        assert_eq!(envelope.data.id, "ses_1");
        assert!(envelope.data.model.is_none());
    }

    #[test]
    fn session_v2_info_deserializes_outcome_and_idle_time() {
        let json = serde_json::json!({
            "id": "ses_1",
            "model": { "id": "m1", "providerID": "p" },
            "outcome": "succeeded",
            "time": { "created": 1, "updated": 2, "idle": 1.5 },
            "location": { "directory": "/wt" },
            "projectID": "p1"
        });
        let info: SessionV2Info = serde_json::from_value(json).unwrap();
        assert_eq!(info.id, "ses_1");
        assert_eq!(info.outcome, Some(SessionOutcome::Succeeded));
        let time = info.time.unwrap();
        assert_eq!(time.created, Some(1.0));
        assert_eq!(time.idle, Some(1.5));
        assert_eq!(
            info.location,
            Some(LocationRef {
                directory: "/wt".into()
            })
        );
        assert_eq!(info.project_id, Some("p1".into()));
    }

    #[test]
    fn session_message_v2_text_extracts_user_flat_and_assistant_content() {
        let user: SessionMessageV2 = serde_json::from_value(serde_json::json!({
            "id": "m1",
            "type": "user",
            "text": "hello",
            "time": { "created": 1 }
        }))
        .unwrap();
        assert!(user.is_user());
        assert_eq!(user.text(), "hello");

        let assistant: SessionMessageV2 = serde_json::from_value(serde_json::json!({
            "id": "m2",
            "type": "assistant",
            "content": [
                { "type": "text", "text": "a" },
                { "type": "reasoning", "text": "x" },
                { "type": "text", "text": "b" }
            ]
        }))
        .unwrap();
        assert!(!assistant.is_user());
        assert_eq!(assistant.text(), "a\nb");

        let other: SessionMessageV2 =
            serde_json::from_value(serde_json::json!({ "type": "shell" })).unwrap();
        assert!(!other.is_user());
        assert_eq!(other.text(), "");
    }

    #[test]
    fn session_messages_response_wraps_data_and_cursor() {
        let json = serde_json::json!({
            "data": [
                { "id": "m1", "type": "user", "text": "hi" },
                { "id": "m2", "type": "assistant", "content": [] }
            ],
            "cursor": { "next": "n1" }
        });
        let resp: SessionMessagesResponse = serde_json::from_value(json).unwrap();
        assert_eq!(resp.data.len(), 2);
        assert_eq!(resp.cursor.unwrap().next, Some("n1".into()));
    }

    #[test]
    fn server_info_deserializes() {
        let json = serde_json::json!({
            "version": "2.0.0",
            "pid": 1234,
            "urls": ["http://localhost:8081"],
            "paths": { "tmp": "/tmp/opencode" }
        });
        let info: ServerInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info.version, "2.0.0");
        assert_eq!(info.pid, 1234);
        assert_eq!(info.urls, vec!["http://localhost:8081".to_string()]);
        assert_eq!(info.paths.tmp, "/tmp/opencode");
    }

    #[test]
    fn location_info_deserializes() {
        let json = serde_json::json!({
            "directory": "/work",
            "project": {
                "id": "p1",
                "directory": "/work",
                "canonical": "/work"
            }
        });
        let info: LocationInfo = serde_json::from_value(json).unwrap();
        assert_eq!(info.directory, "/work");
        assert_eq!(info.project.id, "p1");
        assert_eq!(info.project.canonical, "/work");
    }

    #[test]
    fn active_session_entry_deserializes() {
        let json = serde_json::json!({ "type": "busy" });
        let entry: ActiveSessionEntry = serde_json::from_value(json).unwrap();
        assert_eq!(entry.kind, "busy");
    }

    #[test]
    fn agent_v2_deserializes() {
        let json = serde_json::json!({
            "id": "git-automate-developer",
            "name": "Developer",
            "description": "writes code",
            "mode": "primary",
            "model": { "id": "m1", "providerID": "p" },
            "system": "you are a dev",
            "hidden": false
        });
        let agent: AgentV2 = serde_json::from_value(json).unwrap();
        assert_eq!(agent.id, "git-automate-developer");
        assert_eq!(agent.name, "Developer");
        assert_eq!(agent.description, Some("writes code".into()));
        assert_eq!(agent.mode, Some(AgentMode::Primary));
        assert_eq!(agent.model.unwrap().id, "m1");

        let minimal: AgentV2 = serde_json::from_value(serde_json::json!({
            "id": "a",
            "name": "A"
        }))
        .unwrap();
        assert!(minimal.description.is_none());
        assert!(minimal.mode.is_none());
        assert!(minimal.model.is_none());
        assert!(minimal.system.is_none());
        assert!(minimal.hidden.is_none());
    }
}
