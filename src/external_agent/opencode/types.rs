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

// ─── Session listing (GET /api/session) ────────────────────────────

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
pub struct Session {
    pub id: String,
    pub model: Option<ModelRef>,
    #[serde(rename = "parentID")]
    pub parent_id: Option<String>,
    pub agent: Option<String>,
    pub outcome: Option<SessionOutcome>,
    #[serde(default)]
    pub time: Option<SessionTime>,
    #[serde(default)]
    pub location: Option<LocationRef>,
    #[serde(rename = "projectID")]
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
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

/// Timestamps associated with a session. JSON numbers may be int or float.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SessionTime {
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

/// An agent descriptor from the `GET /api/agent` endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Agent {
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

/// A message entry from the `GET /api/session/{id}/message` endpoint.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SessionMessage {
    User {
        id: String,
        #[serde(default)]
        time: Option<SessionTime>,
        #[serde(default)]
        text: String,
        #[serde(default)]
        delivery: Option<String>,
    },
    Assistant {
        id: String,
        #[serde(default)]
        time: Option<SessionTime>,
        #[serde(default)]
        content: Vec<AssistantContent>,
    },
    #[serde(other)]
    Other,
}

impl SessionMessage {
    pub fn text(&self) -> String {
        match self {
            SessionMessage::User { text, .. } => text.clone(),
            SessionMessage::Assistant { content, .. } => {
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
            SessionMessage::Other => String::new(),
        }
    }

    pub fn is_user(&self) -> bool {
        matches!(self, SessionMessage::User { .. })
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

/// Response wrapper for the `GET /api/session/{id}/message` endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionMessagesResponse {
    pub data: Vec<SessionMessage>,
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
    fn session_deserializes() {
        let json = serde_json::json!({
            "id": "ses_abc123",
            "model": {
                "id": "cuda-small/fast",
                "providerID": "myprovider"
            }
        });
        let info: Session = serde_json::from_value(json).unwrap();
        assert_eq!(info.id, "ses_abc123");
        assert!(info.model.is_some());
        let model = info.model.unwrap();
        assert_eq!(model.id, "cuda-small/fast");
        assert_eq!(model.provider_id, "myprovider");
    }

    #[test]
    fn session_missing_model() {
        let json = serde_json::json!({
            "id": "ses_xyz789"
        });
        let info: Session = serde_json::from_value(json).unwrap();
        assert_eq!(info.id, "ses_xyz789");
        assert!(info.model.is_none());
    }

    #[test]
    fn data_envelope_unwraps() {
        let json = serde_json::json!({ "data": { "id": "ses_1" } });
        let envelope: Data<Session> = serde_json::from_value(json).unwrap();
        assert_eq!(envelope.data.id, "ses_1");
        assert!(envelope.data.model.is_none());
    }

    #[test]
    fn session_deserializes_outcome_and_idle_time() {
        let json = serde_json::json!({
            "id": "ses_1",
            "model": { "id": "m1", "providerID": "p" },
            "outcome": "succeeded",
            "time": { "created": 1, "updated": 2, "idle": 1.5 },
            "location": { "directory": "/wt" },
            "projectID": "p1"
        });
        let info: Session = serde_json::from_value(json).unwrap();
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
    fn session_message_text_extracts_user_flat_and_assistant_content() {
        let user: SessionMessage = serde_json::from_value(serde_json::json!({
            "id": "m1",
            "type": "user",
            "text": "hello",
            "time": { "created": 1 }
        }))
        .unwrap();
        assert!(user.is_user());
        assert_eq!(user.text(), "hello");

        let assistant: SessionMessage = serde_json::from_value(serde_json::json!({
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

        let other: SessionMessage =
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
    fn agent_deserializes() {
        let json = serde_json::json!({
            "id": "git-automate-developer",
            "name": "Developer",
            "description": "writes code",
            "mode": "primary",
            "model": { "id": "m1", "providerID": "p" },
            "system": "you are a dev",
            "hidden": false
        });
        let agent: Agent = serde_json::from_value(json).unwrap();
        assert_eq!(agent.id, "git-automate-developer");
        assert_eq!(agent.name, "Developer");
        assert_eq!(agent.description, Some("writes code".into()));
        assert_eq!(agent.mode, Some(AgentMode::Primary));
        assert_eq!(agent.model.unwrap().id, "m1");

        let minimal: Agent = serde_json::from_value(serde_json::json!({
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
