//! OpenCode implementation of the [`ExternalAgent`] trait.
//!
//! Contains the `impl ExternalAgent for OpenCodeClient` block, the
//! `extract_shell_output` helper, OpenCode-specific deserialisation types
//! (`SessionStatusMap`, `OpenCodeSessionStatus`), and the `From<Session> for
//! SessionInfo` projection. All tests for the trait implementation live here.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::external_agent::common::{
    AgentSessionStatus, ExternalAgent, ExternalAgentError, SessionInfo,
};
use crate::external_agent::opencode::client::OpenCodeClient;
use crate::external_agent::opencode::types::Session;

// ─── From<Session> for SessionInfo ─────────────────────────────

impl From<Session> for SessionInfo {
    fn from(session: Session) -> Self {
        SessionInfo {
            id: session.id,
            title: session.title,
            directory: session.directory,
            project_id: session.project_id,
        }
    }
}

// ─── OpenCode response types (internal) ────────────────────────

/// Deserialiser for `GET /session/status` — a map of session ID → status.
#[derive(Debug, Clone, Deserialize)]
struct SessionStatusMap {
    #[serde(flatten)]
    sessions: std::collections::HashMap<String, OpenCodeSessionStatus>,
}

/// The session status union:
/// `{ type: "idle" } | { type: "busy" } | { type: "retry", ... }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum OpenCodeSessionStatus {
    Idle,
    Busy,
    #[allow(dead_code)]
    Retry {
        attempt: u32,
        message: String,
    },
}

// ─── Helper: extract text output from shell-command response ───

/// Internal helper: extract text output from a shell-command response body.
///
/// The OpenCode `/session/{id}/shell` endpoint returns an *AssistantMessage*
/// whose `parts` array may contain `text` parts and `tool` parts. Tool parts
/// with a `completed` state carry an `output` string with the command result.
fn extract_shell_output(body: &Value) -> String {
    let mut output = String::new();

    let Some(parts) = body.get("parts").and_then(|p| p.as_array()) else {
        // No parts — fall back to the raw body text if it's a simple string.
        if let Some(s) = body.as_str() {
            return s.to_string();
        }
        return String::new();
    };

    for part in parts {
        let part_type = part.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match part_type {
            "text" => {
                if let Some(text) = part.get("text").and_then(|t| t.as_str())
                    && !text.is_empty()
                {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(text);
                }
            }
            "tool" => {
                if let Some(state) = part.get("state").and_then(|s| s.as_object())
                    && state
                        .get("status")
                        .and_then(|s| s.as_str())
                        .is_some_and(|s| s == "completed")
                    && let Some(out) = state.get("output").and_then(|o| o.as_str())
                    && !out.is_empty()
                {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str(out);
                }
            }
            _ => {}
        }
    }

    output
}

// ─── Implementation for OpenCodeClient ─────────────────────────

impl ExternalAgent for OpenCodeClient {
    fn start_session(
        &self,
        project_key: &str,
        system_prompt: &str,
        user_prompt: &str,
    ) -> impl std::future::Future<Output = Result<String, ExternalAgentError>> + Send {
        let base_url = self.base_url.clone();
        let auth_header = self.auth_header.clone();
        let agent = self.agent.clone();
        let http = self.client.clone();

        async move {
            let title = user_prompt.lines().next().unwrap_or("Session").to_string();

            let mut prompt_body = json!({
                "parts": [{ "type": "text", "text": user_prompt }]
            });
            if !system_prompt.is_empty() {
                prompt_body["system"] = json!(system_prompt);
            }
            if let Some(ref agent_name) = agent
                && !agent_name.is_empty()
            {
                prompt_body["agent"] = json!(agent_name);
            }

            crate::external_agent::opencode::client::start_session_http(
                &http,
                &base_url,
                &auth_header,
                project_key,
                None,
                &title,
                prompt_body,
            )
            .await
            .map_err(|e| ExternalAgentError::StartSession(e.to_string()))
        }
    }

    fn execute_shell(
        &self,
        session_id: &str,
        command: &str,
    ) -> impl std::future::Future<Output = Result<String, ExternalAgentError>> + Send {
        let base_url = self.base_url.clone();
        let auth_header = self.auth_header.clone();
        let http = self.client.clone();

        async move {
            let resp = http
                .post(format!("{}/session/{}/shell", base_url, session_id))
                .header("Authorization", &auth_header)
                .json(&json!({ "command": command }))
                .send()
                .await?;

            let status = resp.status();
            if !status.is_success() {
                if status.as_u16() == 404 {
                    return Err(ExternalAgentError::SessionNotFound(session_id.to_string()));
                }
                return Err(ExternalAgentError::ShellCommand(format!(
                    "shell HTTP status {}",
                    status.as_u16()
                )));
            }

            let body: Value = resp.json().await?;
            Ok(extract_shell_output(&body))
        }
    }

    fn session_status(
        &self,
        session_id: &str,
    ) -> impl std::future::Future<Output = Result<AgentSessionStatus, ExternalAgentError>> + Send
    {
        let base_url = self.base_url.clone();
        let auth_header = self.auth_header.clone();
        let http = self.client.clone();
        let session_id_owned = session_id.to_string();

        async move {
            let resp = http
                .get(format!("{}/session/status", base_url))
                .header("Authorization", &auth_header)
                .send()
                .await?;

            let status = resp.status();
            if !status.is_success() {
                return Err(ExternalAgentError::SessionStatus(format!(
                    "session/status HTTP status {}",
                    status.as_u16()
                )));
            }

            let map: SessionStatusMap = resp.json().await.map_err(|e| {
                ExternalAgentError::Parse(format!("failed to decode session status: {}", e))
            })?;

            match map.sessions.get(&session_id_owned) {
                Some(OpenCodeSessionStatus::Idle) => Ok(AgentSessionStatus::Idle),
                Some(OpenCodeSessionStatus::Busy) => Ok(AgentSessionStatus::Waiting),
                Some(OpenCodeSessionStatus::Retry { .. }) => Ok(AgentSessionStatus::Waiting),
                None => Ok(AgentSessionStatus::Done),
            }
        }
    }

    fn nudge_session(
        &self,
        session_id: &str,
    ) -> impl std::future::Future<Output = Result<(), ExternalAgentError>> + Send {
        let base_url = self.base_url.clone();
        let auth_header = self.auth_header.clone();
        let http = self.client.clone();
        let session_id_owned = session_id.to_string();

        async move {
            let resp = http
                .post(format!(
                    "{}/session/{}/prompt_async",
                    base_url, session_id_owned
                ))
                .header("Authorization", &auth_header)
                .json(&json!({
                    "parts": [{ "type": "text", "text": "Please continue." }]
                }))
                .send()
                .await?;

            let status = resp.status();
            if !status.is_success() {
                if status.as_u16() == 404 {
                    return Err(ExternalAgentError::SessionNotFound(session_id_owned));
                }
                return Err(ExternalAgentError::Nudge(format!(
                    "nudge HTTP status {}",
                    status.as_u16()
                )));
            }

            Ok(())
        }
    }

    fn get_session(
        &self,
        session_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<SessionInfo>, ExternalAgentError>> + Send
    {
        let base_url = self.base_url.clone();
        let auth_header = self.auth_header.clone();
        let http = self.client.clone();
        let session_id_owned = session_id.to_string();

        async move {
            let resp = http
                .get(format!("{}/session/{}", base_url, session_id_owned))
                .header("Authorization", &auth_header)
                .send()
                .await?;

            let status = resp.status();
            if status.as_u16() == 404 {
                return Ok(None);
            }
            if !status.is_success() {
                return Err(ExternalAgentError::GetSession(format!(
                    "get session HTTP status {}",
                    status.as_u16()
                )));
            }

            let session: Session = resp.json().await?;
            Ok(Some(SessionInfo::from(session)))
        }
    }

    fn list_sessions(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<SessionInfo>, ExternalAgentError>> + Send
    {
        let base_url = self.base_url.clone();
        let auth_header = self.auth_header.clone();
        let http = self.client.clone();

        async move {
            let resp = http
                .get(format!("{}/session", base_url))
                .header("Authorization", &auth_header)
                .send()
                .await?;

            let status = resp.status();
            if !status.is_success() {
                return Err(ExternalAgentError::ListSessions(format!(
                    "list sessions HTTP status {}",
                    status.as_u16()
                )));
            }

            let sessions: Vec<Session> = resp.json().await?;
            Ok(sessions.into_iter().map(SessionInfo::from).collect())
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_agent::opencode::encode_basic_auth;
    use wiremock::matchers::{body_json, header, method, path, query_param};
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

    // ── start_session ───────────────────────────────────────────

    // Test 1: start_session returns session ID on success
    #[tokio::test]
    async fn start_session_returns_id() {
        let create = Mock::given(method("POST"))
            .and(path("/session"))
            .and(query_param("directory", "/proj"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/proj",
                "title": "Some title",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .expect(1);

        let prompt = Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(204))
            .expect(1);

        let server = MockServer::start().await;
        create.mount(&server).await;
        prompt.mount(&server).await;

        let c = client_with_agent(&server, "git-automate-triage");
        let id = ExternalAgent::start_session(&c, "/proj", "system instructions", "hello world")
            .await
            .unwrap();
        assert_eq!(id, "sess123");
    }

    // Test 2: start_session sends system prompt when non-empty
    #[tokio::test]
    async fn start_session_includes_system_prompt() {
        let create = Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/proj",
                "title": "t",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .expect(1);

        let prompt = Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .and(header("Authorization", AUTH))
            .and(body_json(json!({
                "system": "you are a bot",
                "agent": "git-automate-triage",
                "parts": [{ "type": "text", "text": "hello world" }]
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1);

        let server = MockServer::start().await;
        create.mount(&server).await;
        prompt.mount(&server).await;

        let c = client_with_agent(&server, "git-automate-triage");
        ExternalAgent::start_session(&c, "/proj", "you are a bot", "hello world")
            .await
            .unwrap();
    }

    // Test 3: start_session omits system field when empty
    #[tokio::test]
    async fn start_session_omits_system_when_empty() {
        let create = Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/proj",
                "title": "t",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .expect(1);

        let prompt = Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .and(header("Authorization", AUTH))
            .and(body_json(json!({
                "agent": "git-automate-triage",
                "parts": [{ "type": "text", "text": "hello world" }]
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1);

        let server = MockServer::start().await;
        create.mount(&server).await;
        prompt.mount(&server).await;

        let c = client_with_agent(&server, "git-automate-triage");
        ExternalAgent::start_session(&c, "/proj", "", "hello world")
            .await
            .unwrap();
    }

    // Test 4: start_session 500 on create returns error
    #[tokio::test]
    async fn start_session_create_500_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = ExternalAgent::start_session(&c, "/proj", "sys", "user").await;
        assert!(result.is_err());
        assert!(matches!(result, Err(ExternalAgentError::StartSession(_))));
    }

    // Test 5: start_session prompt_async 500 returns error
    #[tokio::test]
    async fn start_session_prompt_500_returns_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/proj",
                "title": "t",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = ExternalAgent::start_session(&c, "/proj", "sys", "user").await;
        assert!(result.is_err());
        assert!(matches!(result, Err(ExternalAgentError::StartSession(_))));
    }

    // ── execute_shell ───────────────────────────────────────────

    // Test 6: execute_shell returns output from text part
    #[tokio::test]
    async fn execute_shell_returns_text_output() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/sess1/shell"))
            .and(header("Authorization", AUTH))
            .and(body_json(json!({ "command": "git status" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "parts": [
                    { "type": "text", "text": "On branch main\nnothing to commit" }
                ]
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server);
        let output = c.execute_shell("sess1", "git status").await.unwrap();
        assert_eq!(output, "On branch main\nnothing to commit");
    }

    // Test 7: execute_shell returns output from tool part
    #[tokio::test]
    async fn execute_shell_returns_tool_output() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/sess1/shell"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "parts": [
                    {
                        "type": "tool",
                        "state": {
                            "status": "completed",
                            "title": "Shell Executed",
                            "output": "hello from shell"
                        }
                    }
                ]
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let output = c.execute_shell("sess1", "echo hello").await.unwrap();
        assert_eq!(output, "hello from shell");
    }

    // Test 8: execute_shell concatenates text and tool output
    #[tokio::test]
    async fn execute_shell_concatenates_output() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/sess1/shell"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "parts": [
                    { "type": "text", "text": "output line 1" },
                    {
                        "type": "tool",
                        "state": {
                            "status": "completed",
                            "output": "output line 2"
                        }
                    }
                ]
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let output = c.execute_shell("sess1", "cmd").await.unwrap();
        assert_eq!(output, "output line 1\noutput line 2");
    }

    // Test 9: execute_shell 404 returns SessionNotFound
    #[tokio::test]
    async fn execute_shell_404_returns_session_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/missing/shell"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = c.execute_shell("missing", "cmd").await;
        assert!(matches!(
            result,
            Err(ExternalAgentError::SessionNotFound(_))
        ));
    }

    // Test 10: execute_shell 500 returns ShellCommand error
    #[tokio::test]
    async fn execute_shell_500_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/sess1/shell"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = c.execute_shell("sess1", "cmd").await;
        assert!(matches!(result, Err(ExternalAgentError::ShellCommand(_))));
    }

    // Test 11: execute_shell empty parts returns empty string
    #[tokio::test]
    async fn execute_shell_empty_parts_returns_empty() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/sess1/shell"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "parts": []
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let output = c.execute_shell("sess1", "cmd").await.unwrap();
        assert_eq!(output, "");
    }

    // Test 12: extract_shell_output with no parts key
    #[test]
    fn extract_shell_output_no_parts_falls_back_to_string() {
        let body = json!("raw string output");
        assert_eq!(extract_shell_output(&body), "raw string output");
    }

    // Test 13: extract_shell_output with no parts returns empty
    #[test]
    fn extract_shell_output_no_parts_returns_empty() {
        let body = json!({ "foo": "bar" });
        assert_eq!(extract_shell_output(&body), "");
    }

    // ── session_status ──────────────────────────────────────────

    // Test 14: session_status idle → Idle
    #[tokio::test]
    async fn session_status_idle() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session/status"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sess1": { "type": "idle" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server);
        let status = c.session_status("sess1").await.unwrap();
        assert_eq!(status, AgentSessionStatus::Idle);
    }

    // Test 15: session_status busy → Waiting
    #[tokio::test]
    async fn session_status_busy() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sess1": { "type": "busy" }
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let status = c.session_status("sess1").await.unwrap();
        assert_eq!(status, AgentSessionStatus::Waiting);
    }

    // Test 16: session_status retry → Waiting
    #[tokio::test]
    async fn session_status_retry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sess1": { "type": "retry", "attempt": 1, "message": "failed" }
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let status = c.session_status("sess1").await.unwrap();
        assert_eq!(status, AgentSessionStatus::Waiting);
    }

    // Test 17: session_status session not found → Done
    #[tokio::test]
    async fn session_status_not_found_done() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "other": { "type": "idle" }
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let status = c.session_status("sess1").await.unwrap();
        assert_eq!(status, AgentSessionStatus::Done);
    }

    // Test 18: session_status empty map → Done
    #[tokio::test]
    async fn session_status_empty_map_done() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        let c = client(&server);
        let status = c.session_status("sess1").await.unwrap();
        assert_eq!(status, AgentSessionStatus::Done);
    }

    // Test 19: session_status 500 returns error
    #[tokio::test]
    async fn session_status_500_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = c.session_status("sess1").await;
        assert!(matches!(result, Err(ExternalAgentError::SessionStatus(_))));
    }

    // ── nudge_session ───────────────────────────────────────────

    // Test 20: nudge_session sends prompt_async with continuation
    #[tokio::test]
    async fn nudge_session_sends_prompt_async() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/sess1/prompt_async"))
            .and(header("Authorization", AUTH))
            .and(body_json(json!({
                "parts": [{ "type": "text", "text": "Please continue." }]
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server);
        c.nudge_session("sess1").await.unwrap();
    }

    // Test 21: nudge_session 404 returns SessionNotFound
    #[tokio::test]
    async fn nudge_session_404_returns_session_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/missing/prompt_async"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = c.nudge_session("missing").await;
        assert!(matches!(
            result,
            Err(ExternalAgentError::SessionNotFound(_))
        ));
    }

    // Test 22: nudge_session 500 returns Nudge error
    #[tokio::test]
    async fn nudge_session_500_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/session/sess1/prompt_async"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = c.nudge_session("sess1").await;
        assert!(matches!(result, Err(ExternalAgentError::Nudge(_))));
    }

    // ── get_session ─────────────────────────────────────────────

    // Test 23: get_session returns Some(SessionInfo) on 200
    #[tokio::test]
    async fn get_session_returns_some() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session/sess1"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess1",
                "projectID": "p1",
                "directory": "/proj",
                "title": "My Session",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server);
        let info = c.get_session("sess1").await.unwrap();
        assert_eq!(info.unwrap().id, "sess1");
    }

    // Test 24: get_session returns None on 404
    #[tokio::test]
    async fn get_session_returns_none_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = c.get_session("missing").await.unwrap();
        assert!(result.is_none());
    }

    // Test 25: get_session 500 returns GetSession error
    #[tokio::test]
    async fn get_session_500_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session/sess1"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = c.get_session("sess1").await;
        assert!(matches!(result, Err(ExternalAgentError::GetSession(_))));
    }

    // ── list_sessions ───────────────────────────────────────────

    // Test 26: list_sessions returns all sessions
    #[tokio::test]
    async fn list_sessions_returns_all() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "id": "sess1",
                    "projectID": "p1",
                    "directory": "/proj1",
                    "title": "Session 1",
                    "version": "1",
                    "time": { "created": 1, "updated": 2 }
                },
                {
                    "id": "sess2",
                    "projectID": "p2",
                    "directory": "/proj2",
                    "title": "Session 2",
                    "version": "1",
                    "time": { "created": 3, "updated": 4 }
                }
            ])))
            .expect(1)
            .mount(&server)
            .await;

        let c = client(&server);
        let sessions = c.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, "sess1");
        assert_eq!(sessions[0].title, "Session 1");
        assert_eq!(sessions[0].directory, "/proj1");
        assert_eq!(sessions[0].project_id, "p1");
        assert_eq!(sessions[1].id, "sess2");
        assert_eq!(sessions[1].title, "Session 2");
    }

    // Test 27: list_sessions empty returns empty vec
    #[tokio::test]
    async fn list_sessions_empty() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let c = client(&server);
        let sessions = c.list_sessions().await.unwrap();
        assert!(sessions.is_empty());
    }

    // Test 28: list_sessions 500 returns error
    #[tokio::test]
    async fn list_sessions_500_returns_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let c = client(&server);
        let result = c.list_sessions().await;
        assert!(matches!(result, Err(ExternalAgentError::ListSessions(_))));
    }

    // ── SessionInfo / Session conversion ────────────────────────

    // Test 29: SessionInfo From<Session>
    #[test]
    fn session_info_from_session() {
        let session = Session {
            id: "sess1".to_string(),
            project_id: "p1".to_string(),
            directory: "/proj".to_string(),
            title: "Title".to_string(),
            version: "1".to_string(),
            time: crate::external_agent::opencode::types::SessionTime {
                created: 1,
                updated: 2,
            },
        };
        let info = SessionInfo::from(session);
        assert_eq!(info.id, "sess1");
        assert_eq!(info.title, "Title");
        assert_eq!(info.directory, "/proj");
        assert_eq!(info.project_id, "p1");
    }

    // Test 30: SessionInfo clone + equality
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

    // ── extract_shell_output (direct unit tests) ────────────────

    // Test 31: extract with empty text part
    #[test]
    fn extract_shell_output_skips_empty_text() {
        let body = json!({
            "parts": [
                { "type": "text", "text": "" },
                { "type": "text", "text": "real" }
            ]
        });
        assert_eq!(extract_shell_output(&body), "real");
    }

    // Test 32: extract with no completed tool state (running)
    #[test]
    fn extract_shell_output_skips_running_tool() {
        let body = json!({
            "parts": [
                {
                    "type": "tool",
                    "state": { "status": "running", "title": "exec" }
                },
                { "type": "text", "text": "done" }
            ]
        });
        assert_eq!(extract_shell_output(&body), "done");
    }

    // Test 33: extract with unknown part type
    #[test]
    fn extract_shell_output_ignores_unknown_parts() {
        let body = json!({
            "parts": [
                { "type": "unknown", "data": "x" },
                { "type": "text", "text": "visible" }
            ]
        });
        assert_eq!(extract_shell_output(&body), "visible");
    }

    // ── Auth header on all agent endpoints ──────────────────────

    // Test 34: auth header sent on all agent endpoints
    #[tokio::test]
    async fn auth_header_sent_on_all_endpoints() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/session"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "s1", "projectID": "p", "directory": "/d", "title": "t",
                "version": "1", "time": { "created": 1, "updated": 2 }
            })))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/session/s1/prompt_async"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/session/s1/shell"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "parts": []
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/session/status"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/session/s1/prompt_async"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/session/s1"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "s1", "projectID": "p", "directory": "/d", "title": "t",
                "version": "1", "time": { "created": 1, "updated": 2 }
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/session"))
            .and(header("Authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;

        let c = client(&server);

        ExternalAgent::start_session(&c, "/d", "sys", "user")
            .await
            .unwrap();
        c.execute_shell("s1", "cmd").await.unwrap();
        c.session_status("s1").await.unwrap();
        c.nudge_session("s1").await.unwrap();
        c.get_session("s1").await.unwrap();
        c.list_sessions().await.unwrap();

        server.verify().await;
    }
}
