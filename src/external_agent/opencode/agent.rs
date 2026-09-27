//! OpenCode implementation of the [`ExternalAgent`] trait.
//!
//! Contains the `impl ExternalAgent for OpenCodeClient` block against the
//! OpenCode v2 HTTP API (`POST /api/session`, `POST /api/session/{id}/prompt`,
//! `GET /api/session/active`, `GET /api/session/{id}`) and the
//! `From<Session> for SessionInfo` projection. All tests for the trait
//! implementation live here.

use crate::external_agent::common::{
    AgentSessionStatus, ExternalAgent, ExternalAgentError, SessionInfo,
};
use crate::external_agent::opencode::client::OpenCodeClient;
use crate::external_agent::opencode::types::{Session, SessionOutcome};

// ─── From<Session> for SessionInfo ───────────────────────

impl From<Session> for SessionInfo {
    fn from(session: Session) -> Self {
        SessionInfo {
            id: session.id,
            title: session.title.unwrap_or_default(),
            directory: session.location.map(|l| l.directory).unwrap_or_default(),
            project_id: session.project_id.unwrap_or_default(),
        }
    }
}

// ─── Implementation for OpenCodeClient ─────────────────────────

impl ExternalAgent for OpenCodeClient {
    async fn start_session(
        &self,
        project_key: &str,
        user_prompt: &str,
    ) -> Result<String, ExternalAgentError> {
        let title = user_prompt.lines().next().unwrap_or("Session").to_string();
        let agent = self.agent.as_deref().unwrap_or("build");

        let session = self
            .create_session(&title, agent, None, project_key)
            .await
            .map_err(|e| ExternalAgentError::StartSession(e.to_string()))?;

        self.send_prompt(&session.id, user_prompt)
            .await
            .map_err(|e| ExternalAgentError::StartSession(e.to_string()))?;

        Ok(session.id)
    }

    async fn session_status(
        &self,
        session_id: &str,
    ) -> Result<AgentSessionStatus, ExternalAgentError> {
        let active = self
            .get_active_sessions()
            .await
            .map_err(|e| ExternalAgentError::SessionStatus(e.to_string()))?;

        if active.contains_key(session_id) {
            return Ok(AgentSessionStatus::Waiting);
        }

        match self
            .get_session_v2(session_id)
            .await
            .map_err(|e| ExternalAgentError::SessionStatus(e.to_string()))?
        {
            None => Ok(AgentSessionStatus::Failed),
            Some(s) => match s.outcome {
                Some(SessionOutcome::Succeeded) => Ok(AgentSessionStatus::Done),
                Some(SessionOutcome::Failed | SessionOutcome::Interrupted) => {
                    Ok(AgentSessionStatus::Failed)
                }
                None => {
                    if s.time.as_ref().and_then(|t| t.idle).is_some() {
                        Ok(AgentSessionStatus::Done)
                    } else {
                        Ok(AgentSessionStatus::Waiting)
                    }
                }
            },
        }
    }

    async fn get_session(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionInfo>, ExternalAgentError> {
        let session = self
            .get_session_v2(session_id)
            .await
            .map_err(|e| ExternalAgentError::GetSession(e.to_string()))?;
        Ok(session.map(SessionInfo::from))
    }
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_agent::opencode::encode_basic_auth;
    use serde_json::json;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a client pointed at a mock server with a known password and agent.
    fn client_with_agent(server: &MockServer, agent: &str) -> OpenCodeClient {
        let mut c = OpenCodeClient::new(server.uri(), "pw".to_string());
        c.set_agent(agent);
        c
    }

    fn client(server: &MockServer) -> OpenCodeClient {
        OpenCodeClient::new(server.uri(), "pw".to_string())
    }

    const AUTH: &str = "Basic b3BlbmNvZGU6cHc="; // base64("opencode:pw")

    // ── encode_basic_auth parity ───────────────────────────────

    #[test]
    fn auth_header_matches() {
        assert_eq!(encode_basic_auth("opencode", "pw"), AUTH);
    }

    // ── start_session (v2) ──────────────────────────────────────

    // Test 1: start_session returns session ID on success; create body carries
    // the agent (and no `system` key — v2 has no per-session system prompt);
    // prompt body is exactly `{"text": ...}`.
    #[tokio::test]
    async fn start_session_returns_id() {
        let create = Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(header("Authorization", AUTH))
            .and(body_json(json!({
                "title": "hello world",
                "agent": "git-automate-triage",
                "location": { "directory": "/proj" }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "id": "ses123",
                    "title": "hello world",
                    "location": { "directory": "/proj" },
                    "projectID": "p1"
                }
            })))
            .expect(1);

        let prompt = Mock::given(method("POST"))
            .and(path("/api/session/ses123/prompt"))
            .and(header("Authorization", AUTH))
            .and(body_json(json!({ "text": "hello world" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "msg_1" }
            })))
            .expect(1);

        let server = MockServer::start().await;
        create.mount(&server).await;
        prompt.mount(&server).await;

        let c = client_with_agent(&server, "git-automate-triage");
        let id = ExternalAgent::start_session(&c, "/proj", "hello world")
            .await
            .unwrap();
        assert_eq!(id, "ses123");
    }

    // Test 2: session title is the first line of a multi-line user prompt.
    #[tokio::test]
    async fn start_session_title_is_first_line() {
        let create = Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(body_json(json!({
                "title": "Fix the bug",
                "agent": "git-automate-triage",
                "location": { "directory": "/proj" }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses999" }
            })))
            .expect(1);

        let prompt = Mock::given(method("POST"))
            .and(path("/api/session/ses999/prompt"))
            .and(body_json(json!({ "text": "Fix the bug\n\nDetails here." })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "msg_1" }
            })))
            .expect(1);

        let server = MockServer::start().await;
        create.mount(&server).await;
        prompt.mount(&server).await;

        let c = client_with_agent(&server, "git-automate-triage");
        let id = ExternalAgent::start_session(&c, "/proj", "Fix the bug\n\nDetails here.")
            .await
            .unwrap();
        assert_eq!(id, "ses999");
    }

    // Test 3: default agent "build" used when the client has no configured agent.
    #[tokio::test]
    async fn start_session_defaults_to_build_agent() {
        let create = Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(body_json(json!({
                "title": "hello",
                "agent": "build",
                "location": { "directory": "/proj" }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses1" }
            })))
            .expect(1);

        let prompt = Mock::given(method("POST"))
            .and(path("/api/session/ses1/prompt"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "msg_1" }
            })))
            .expect(1);

        let server = MockServer::start().await;
        create.mount(&server).await;
        prompt.mount(&server).await;

        let c = client(&server);
        ExternalAgent::start_session(&c, "/proj", "hello")
            .await
            .unwrap();
    }

    // Test 4: start_session 500 on create returns StartSession error.
    #[tokio::test]
    async fn start_session_create_500_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = ExternalAgent::start_session(&c, "/proj", "user").await;
        assert!(matches!(result, Err(ExternalAgentError::StartSession(_))));
    }

    // Test 5: start_session 500 on prompt returns StartSession error.
    #[tokio::test]
    async fn start_session_prompt_500_returns_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses123" }
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/api/session/ses123/prompt"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = ExternalAgent::start_session(&c, "/proj", "user").await;
        assert!(matches!(result, Err(ExternalAgentError::StartSession(_))));
    }

    // ── session_status (v2) ─────────────────────────────────────

    // Test 6: session present in the active map → Waiting (no v2 fetch needed).
    #[tokio::test]
    async fn session_status_active_returns_waiting() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "ses1": { "type": "busy" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Must not be called: active short-circuits.
        Mock::given(method("GET"))
            .and(path("/api/session/ses1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses1" }
            })))
            .expect(0)
            .mount(&server)
            .await;

        let c = client(&server);
        let status = c.session_status("ses1").await.unwrap();
        assert_eq!(status, AgentSessionStatus::Waiting);
    }

    // Test 7: not active + outcome=succeeded → Done.
    #[tokio::test]
    async fn session_status_outcome_returns_done() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/session/ses1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses1", "outcome": "succeeded" }
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let status = c.session_status("ses1").await.unwrap();
        assert_eq!(status, AgentSessionStatus::Done);
    }

    // Test 8: not active + idle timestamp (no outcome) → Done.
    #[tokio::test]
    async fn session_status_idle_time_returns_done() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/session/ses1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses1", "time": { "created": 1, "idle": 1.5 } }
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let status = c.session_status("ses1").await.unwrap();
        assert_eq!(status, AgentSessionStatus::Done);
    }

    // Test 9: not active + no outcome and no idle timestamp → Waiting.
    #[tokio::test]
    async fn session_status_no_outcome_no_idle_returns_waiting() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/session/ses1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses1", "time": { "created": 1, "updated": 2 } }
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let status = c.session_status("ses1").await.unwrap();
        assert_eq!(status, AgentSessionStatus::Waiting);
    }

    // Test 10: not active + 404 on v2 fetch → Failed (a missing session is not success).
    #[tokio::test]
    async fn session_status_404_returns_failed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/session/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let c = client(&server);
        let status = c.session_status("missing").await.unwrap();
        assert_eq!(status, AgentSessionStatus::Failed);
    }

    // Test 10b: not active + outcome=failed → Failed (not Done).
    #[tokio::test]
    async fn session_status_outcome_failed_returns_failed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/session/ses1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses1", "outcome": "failed" }
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let status = c.session_status("ses1").await.unwrap();
        assert_eq!(status, AgentSessionStatus::Failed);
    }

    // Test 11: session_status 500 on active fetch returns SessionStatus error.
    #[tokio::test]
    async fn session_status_active_500_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = c.session_status("ses1").await;
        assert!(matches!(result, Err(ExternalAgentError::SessionStatus(_))));
    }

    // ── get_session (v2) ────────────────────────────────────────

    // Test 12: get_session returns Some(SessionInfo) on 200.
    #[tokio::test]
    async fn get_session_returns_some() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/session/ses1"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "id": "ses1",
                    "title": "My Session",
                    "location": { "directory": "/proj" },
                    "projectID": "p1"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server);
        let info = c.get_session("ses1").await.unwrap().unwrap();
        assert_eq!(info.id, "ses1");
        assert_eq!(info.title, "My Session");
        assert_eq!(info.directory, "/proj");
        assert_eq!(info.project_id, "p1");
    }

    // Test 13: get_session returns None on 404.
    #[tokio::test]
    async fn get_session_returns_none_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/session/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = c.get_session("missing").await.unwrap();
        assert!(result.is_none());
    }

    // Test 14: get_session 500 returns GetSession error.
    #[tokio::test]
    async fn get_session_500_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/session/ses1"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = c.get_session("ses1").await;
        assert!(matches!(result, Err(ExternalAgentError::GetSession(_))));
    }

    // ── SessionInfo / Session conversion ──────────────────

    // Test 15: SessionInfo From<Session> with all fields populated.
    #[test]
    fn session_info_from_session_v2_full() {
        let session: Session = serde_json::from_value(json!({
            "id": "ses1",
            "title": "Title",
            "location": { "directory": "/proj" },
            "projectID": "p1"
        }))
        .unwrap();
        let info = SessionInfo::from(session);
        assert_eq!(info.id, "ses1");
        assert_eq!(info.title, "Title");
        assert_eq!(info.directory, "/proj");
        assert_eq!(info.project_id, "p1");
    }

    // Test 16: SessionInfo From<Session> with missing optional fields
    // falls back to empty strings.
    #[test]
    fn session_info_from_session_v2_minimal_defaults() {
        let session: Session = serde_json::from_value(json!({ "id": "ses1" })).unwrap();
        let info = SessionInfo::from(session);
        assert_eq!(info.id, "ses1");
        assert_eq!(info.title, "");
        assert_eq!(info.directory, "");
        assert_eq!(info.project_id, "");
    }

    // Test 17: SessionInfo clone + equality.
    #[test]
    fn session_info_clone_and_equality() {
        let info = SessionInfo {
            id: "s1".to_string(),
            title: "t".to_string(),
            directory: "d".to_string(),
            project_id: "p".to_string(),
        };
        let cloned = info.clone();
        assert_eq!(info, cloned);
    }
}
