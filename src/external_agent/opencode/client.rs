use base64::Engine;
use reqwest::Client;
use serde_json::json;
use thiserror::Error;

use crate::external_agent::opencode::types::{
    Agent, AgentInfo, HealthResponse, Session, SessionMessage, Workspace, Worktree,
};

const OPENCODE_USERNAME: &str = "opencode";

/// Errors that can occur while communicating with the OpenCode HTTP API.
#[derive(Debug, Clone, Error)]
pub enum OpenCodeError {
    #[error("HTTP error: {0}")]
    Http(String),
    #[error("HTTP status {0}")]
    HttpStatus(u16),
    #[error("Failed to fetch agents: {0}")]
    FetchAgents(String),
    #[error("Failed to create session: {0}")]
    CreateSession(String),
    #[error("Session creation returned no data")]
    NoSessionData,
    #[error("Failed to fetch session messages: {0}")]
    FetchSessionMessages(String),
    #[error("Failed to create workspace: {0}")]
    CreateWorkspace(String),
    #[error("Failed to create worktree: {0}")]
    CreateWorktree(String),
}

impl From<reqwest::Error> for OpenCodeError {
    fn from(err: reqwest::Error) -> Self {
        OpenCodeError::Http(err.to_string())
    }
}

/// A minimal HTTP client for the OpenCode server.
///
/// Uses HTTP Basic auth with username `"opencode"` and the provided password.
#[derive(Debug, Clone)]
pub struct OpenCodeClient {
    pub(crate) client: Client,
    pub(crate) base_url: String,
    pub(crate) auth_header: String,
    /// Optional default agent name (e.g. `"git-automate-triage"`).
    pub(crate) agent: Option<String>,
}

impl OpenCodeClient {
    /// Create a new client targeting `url` with the given `password`.
    ///
    /// The `Authorization` header is precomputed once as
    /// `Basic base64("opencode:<password>")`. The `agent` field is initialised
    /// to `None`; use [`set_agent`](Self::set_agent) to specify a default agent.
    pub fn new(url: String, password: String) -> Self {
        let auth_header = encode_basic_auth(OPENCODE_USERNAME, &password);
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client with valid timeout");
        Self {
            client,
            base_url: url,
            auth_header,
            agent: None,
        }
    }

    pub fn set_agent(&mut self, agent: &str) {
        self.agent = Some(agent.to_string());
    }

    pub fn with_agent(url: String, password: String, agent: &str) -> Self {
        let mut client = Self::new(url, password);
        client.set_agent(agent);
        client
    }

    /// `GET /global/health` — returns `true` only when the server reports
    /// `healthy: true`.
    ///
    /// On *any* failure (network error, non-2xx status, malformed JSON, or a
    /// missing `healthy` field)
    /// the method returns `false` rather than propagating the error.
    pub async fn check_health(&self) -> bool {
        self.check_health_inner().await.unwrap_or_default()
    }

    async fn check_health_inner(&self) -> Result<bool, OpenCodeError> {
        let response = self
            .client
            .get(format!("{}/global/health", self.base_url))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        if !response.status().is_success() {
            return Ok(false);
        }
        let health: HealthResponse = response.json().await?;
        Ok(health.healthy)
    }

    /// `GET /agent` — list available agents.
    ///
    /// When `directory` is `Some`, it is appended as a `?directory=<dir>` query
    /// parameter, matching the SDK's `client.app.agents({ directory })`.
    pub async fn get_agents(
        &self,
        directory: Option<&str>,
    ) -> Result<Vec<AgentInfo>, OpenCodeError> {
        let mut request = self
            .client
            .get(format!("{}/agent", self.base_url))
            .header("Authorization", &self.auth_header);
        if let Some(dir) = directory {
            request = request.query(&[("directory", dir)]);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(OpenCodeError::HttpStatus(response.status().as_u16()));
        }
        let agents: Vec<Agent> = response.json().await?;
        Ok(agents.into_iter().map(AgentInfo::from).collect())
    }

    /// Create a session and immediately send it a prompt.
    ///
    /// 1. `POST /session?directory=<dir>` with body `{"title": <title>}` to
    ///    obtain a [`Session`]; its `id` is returned on success.
    /// 2. `POST /session/<id>/prompt_async` with body
    ///    `{"agent": <agent>, "parts": [{"type": "text", "text": <message>}]}`
    ///    to deliver the prompt. The 204 response body is ignored.
    pub async fn start_session(
        &self,
        directory: &str,
        title: &str,
        agent: &str,
        message: &str,
    ) -> Result<String, OpenCodeError> {
        let prompt_body = json!({
            "agent": agent,
            "parts": [{ "type": "text", "text": message }]
        });
        start_session_http(
            &self.client,
            &self.base_url,
            &self.auth_header,
            directory,
            None,
            title,
            prompt_body,
        )
        .await
    }

    /// Create a session and send a prompt with an optional system prompt.
    ///
    /// Like [`start_session`](Self::start_session) but uses a `system` field
    /// for instructions and an *optional* `agent` field. When `agent` is empty
    /// the field is omitted, so OpenCode uses its default agent with the
    /// provided `system` instructions — enabling prompt construction from
    /// embedded markdown without pre-installed agents.
    pub async fn start_session_with_system(
        &self,
        directory: &str,
        title: &str,
        system_prompt: &str,
        agent: &str,
        message: &str,
        workspace: Option<&str>,
    ) -> Result<String, OpenCodeError> {
        let mut prompt_body = json!({
            "parts": [{ "type": "text", "text": message }]
        });
        if !system_prompt.is_empty() {
            prompt_body["system"] = json!(system_prompt);
        }
        if !agent.is_empty() {
            prompt_body["agent"] = json!(agent);
        }
        start_session_http(
            &self.client,
            &self.base_url,
            &self.auth_header,
            directory,
            workspace,
            title,
            prompt_body,
        )
        .await
    }

    /// `GET /session/status` — count active (non-Done) sessions.
    /// Sessions present in the status map (idle, busy, retry) are considered active.
    /// Sessions absent from the map are Done and do not count.
    pub async fn count_active_sessions(&self) -> Result<usize, OpenCodeError> {
        self.get_session_statuses().await.map(|m| m.len())
    }

    /// `GET /session/status` — return all active session IDs and their statuses.
    /// Session IDs absent from the returned map have completed.
    pub async fn get_session_statuses(
        &self,
    ) -> Result<std::collections::HashMap<String, serde_json::Value>, OpenCodeError> {
        let response = self
            .client
            .get(format!("{}/session/status", self.base_url))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(OpenCodeError::HttpStatus(response.status().as_u16()));
        }
        let statuses: std::collections::HashMap<String, serde_json::Value> =
            response.json().await?;
        Ok(statuses)
    }

    /// `GET /session/{id}/message` — list messages in a session.
    ///
    /// Each entry has `info` (id, role, sessionID, time) and `parts` (content).
    /// Use [`SessionMessage::text`] to extract prompt text.
    pub async fn get_session_messages(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<Vec<SessionMessage>, OpenCodeError> {
        let mut request = self
            .client
            .get(format!("{}/session/{}/message", self.base_url, session_id))
            .header("Authorization", &self.auth_header);
        if let Some(dir) = directory {
            request = request.query(&[("directory", dir)]);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(OpenCodeError::FetchSessionMessages(format!(
                "HTTP status {}",
                response.status().as_u16()
            )));
        }
        let messages: Vec<SessionMessage> = response.json().await?;
        Ok(messages)
    }

    /// `POST /experimental/workspace` — create a new workspace.
    ///
    /// Sends `{"type": "git"}` as the body with `directory` as a query param.
    /// Returns the created [`Workspace`] on success.
    pub async fn create_workspace(&self, directory: &str) -> Result<Workspace, OpenCodeError> {
        let response = self
            .client
            .post(format!("{}/experimental/workspace", self.base_url))
            .query(&[("directory", directory)])
            .header("Authorization", &self.auth_header)
            .json(&json!({ "type": "git" }))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(OpenCodeError::CreateWorkspace(format!(
                "workspace creation HTTP status {}",
                response.status().as_u16()
            )));
        }
        let workspace: Workspace = response.json().await?;
        Ok(workspace)
    }

    /// `POST /experimental/worktree` — create a new git worktree.
    ///
    /// Uses `directory` and `workspace_id` as query params with an empty JSON
    /// body. Returns the created [`Worktree`] on success.
    pub async fn create_worktree(
        &self,
        directory: &str,
        workspace_id: &str,
    ) -> Result<Worktree, OpenCodeError> {
        let response = self
            .client
            .post(format!("{}/experimental/worktree", self.base_url))
            .query(&[("directory", directory), ("workspace", workspace_id)])
            .header("Authorization", &self.auth_header)
            .json(&json!({}))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(OpenCodeError::CreateWorktree(format!(
                "worktree creation HTTP status {}",
                response.status().as_u16()
            )));
        }
        let worktree: Worktree = response.json().await?;
        Ok(worktree)
    }
}

/// Shared two-step HTTP flow used by [`OpenCodeClient::start_session`] and the
/// `ExternalAgent` trait impl to create a session and deliver a prompt.
pub(crate) async fn start_session_http(
    client: &Client,
    base_url: &str,
    auth_header: &str,
    directory: &str,
    workspace: Option<&str>,
    title: &str,
    prompt_body: serde_json::Value,
) -> Result<String, OpenCodeError> {
    let mut query = vec![("directory", directory)];
    if let Some(ws) = workspace {
        query.push(("workspace", ws));
    }
    let create_response = client
        .post(format!("{}/session", base_url))
        .query(&query)
        .header("Authorization", auth_header)
        .json(&json!({ "title": title }))
        .send()
        .await?;

    if !create_response.status().is_success() {
        return Err(OpenCodeError::CreateSession(format!(
            "session creation HTTP status {}",
            create_response.status().as_u16()
        )));
    }

    let session: Session = create_response.json().await?;
    if session.id.is_empty() {
        return Err(OpenCodeError::NoSessionData);
    }
    let session_id = session.id;

    let prompt_response = client
        .post(format!("{}/session/{}/prompt_async", base_url, session_id))
        .header("Authorization", auth_header)
        .json(&prompt_body)
        .send()
        .await?;

    if !prompt_response.status().is_success() {
        return Err(OpenCodeError::CreateSession(format!(
            "prompt_async HTTP status {}",
            prompt_response.status().as_u16()
        )));
    }

    Ok(session_id)
}

/// Encode credentials for HTTP Basic authentication.
///
/// Returns `Basic <base64("username:password")>`, using standard base64
/// encoding.
pub fn encode_basic_auth(username: &str, password: &str) -> String {
    let combined = format!("{}:{}", username, password);
    let encoded = base64::engine::general_purpose::STANDARD.encode(combined);
    format!("Basic {}", encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Build a client pointed at a mock server with a known password.
    fn client(server: &MockServer) -> OpenCodeClient {
        OpenCodeClient::new(server.uri(), "pw".to_string())
    }

    // --- encode_basic_auth (test 1) -------------------------------------

    #[test]
    fn test_encode_basic_auth_basic() {
        let encoded = encode_basic_auth("opencode", "secret");
        assert_eq!(encoded, "Basic b3BlbmNvZGU6c2VjcmV0");
    }

    #[test]
    fn test_encode_basic_auth_arbitrary_password() {
        // base64("opencode:mypassword") == b3BlbmNvZGU6bXlwYXNzd29yZA==
        let encoded = encode_basic_auth("opencode", "mypassword");
        assert_eq!(encoded, "Basic b3BlbmNvZGU6bXlwYXNzd29yZA==");
    }

    // --- check_health (tests 2-6) ---------------------------------------

    #[tokio::test]
    async fn test_check_health_healthy() {
        let mock = Mock::given(method("GET"))
            .and(path("/global/health"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "healthy": true, "version": "1.0.0" })),
            )
            .expect(1)
            .named("health");
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        assert!(client.check_health().await);
    }

    #[tokio::test]
    async fn test_check_health_unhealthy() {
        let mock = Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "healthy": false, "version": "1.0.0" })),
            )
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        assert!(!client.check_health().await);
    }

    #[tokio::test]
    async fn test_check_health_500_returns_false() {
        let mock = Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        assert!(!client.check_health().await);
    }

    #[tokio::test]
    async fn test_check_health_malformed_json_returns_false() {
        let mock = Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("not json", "text/plain"))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        assert!(!client.check_health().await);
    }

    #[tokio::test]
    async fn test_check_health_missing_healthy_field_returns_false() {
        let mock = Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": "1.0.0" })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        assert!(!client.check_health().await);
    }

    #[tokio::test]
    async fn test_check_health_network_error_returns_false() {
        // No mocks mounted — every request gets an immediate connection reset.
        let server = MockServer::start().await;
        let client = client(&server);
        assert!(!client.check_health().await);
    }

    // --- get_agents (tests 7-10) ----------------------------------------

    #[tokio::test]
    async fn test_get_agents_no_description() {
        let mock = Mock::given(method("GET"))
            .and(path("/agent"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "name": "agent1", "mode": "subagent", "native": true }
            ])))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let agents = client.get_agents(None).await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "agent1");
        assert_eq!(agents[0].description, None);
    }

    #[tokio::test]
    async fn test_get_agents_with_description() {
        let mock = Mock::given(method("GET"))
            .and(path("/agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "name": "a",
                    "mode": "primary",
                    "native": false,
                    "description": "test"
                }
            ])))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let agents = client.get_agents(None).await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "a");
        assert_eq!(agents[0].description.as_deref(), Some("test"));
    }

    #[tokio::test]
    async fn test_get_agents_with_directory_query_param() {
        let mock = Mock::given(method("GET"))
            .and(path("/agent"))
            .and(query_param("directory", "/path"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let agents = client.get_agents(Some("/path")).await.unwrap();
        assert!(agents.is_empty());
    }

    #[tokio::test]
    async fn test_get_agents_no_directory_omits_query_param() {
        // If a `directory` param is present the request must NOT match this mock
        // (wiremock returns 404 for unmatched requests, surfacing a real error).
        let mock = Mock::given(method("GET"))
            .and(path("/agent"))
            .and(query_param("directory", "/absent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(0);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        // Mount a fallback that matches the no-query-param request.
        let ok = Mock::given(method("GET"))
            .and(path("/agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1);
        ok.mount(&server).await;

        let client = client(&server);
        client.get_agents(None).await.unwrap();
    }

    // --- start_session (tests 11-14) ------------------------------------

    #[tokio::test]
    async fn test_start_session_returns_id() {
        // Session creation mock.
        let create = Mock::given(method("POST"))
            .and(path("/session"))
            .and(query_param("directory", "/d"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .expect(1);
        let server = MockServer::start().await;
        create.mount(&server).await;

        // prompt_async mock.
        let prompt = Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1);
        prompt.mount(&server).await;

        let client = client(&server);
        let id = client
            .start_session("/d", "Session Title", "git-automate-triage", "issue body")
            .await
            .unwrap();
        assert_eq!(id, "sess123");
    }

    #[tokio::test]
    async fn test_start_session_500_on_create_returns_error() {
        let create = Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1);
        let server = MockServer::start().await;
        create.mount(&server).await;

        let client = client(&server);
        let result = client
            .start_session("/d", "t", "git-automate-triage", "m")
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_start_session_prompt_async_body() {
        let create = Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .expect(1);
        let server = MockServer::start().await;
        create.mount(&server).await;

        let prompt = Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .and(body_json(json!({
                "agent": "git-automate-triage",
                "parts": [{ "type": "text", "text": "issue body" }]
            })))
            .respond_with(ResponseTemplate::new(204))
            .expect(1);
        prompt.mount(&server).await;

        let client = client(&server);
        client
            .start_session("/d", "Session Title", "git-automate-triage", "issue body")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_start_session_create_body_is_title() {
        let create = Mock::given(method("POST"))
            .and(path("/session"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .and(body_json(json!({ "title": "Session Title" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .expect(1);
        let server = MockServer::start().await;
        create.mount(&server).await;

        let prompt = Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1);
        prompt.mount(&server).await;

        let client = client(&server);
        client
            .start_session("/d", "Session Title", "git-automate-triage", "issue body")
            .await
            .unwrap();
    }

    // --- auth header on all requests (test 15) --------------------------

    #[tokio::test]
    async fn test_auth_header_sent_on_all_requests() {
        // health
        let health = Mock::given(method("GET"))
            .and(path("/global/health"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "healthy": true, "version": "1" })),
            )
            .expect(1)
            .named("health");
        // agent
        let agent = Mock::given(method("GET"))
            .and(path("/agent"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1)
            .named("agent");
        // session create
        let create = Mock::given(method("POST"))
            .and(path("/session"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess1",
                "projectID": "p",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .expect(1)
            .named("create");
        // prompt_async
        let prompt = Mock::given(method("POST"))
            .and(path("/session/sess1/prompt_async"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .named("prompt");

        let server = MockServer::start().await;
        health.mount(&server).await;
        agent.mount(&server).await;
        create.mount(&server).await;
        prompt.mount(&server).await;

        let client = client(&server);
        assert!(client.check_health().await);
        client.get_agents(None).await.unwrap();
        client
            .start_session("/d", "t", "git-automate-triage", "m")
            .await
            .unwrap();

        server.verify().await;
    }

    // --- count_active_sessions (tests 16-19) ------------------------------

    #[tokio::test]
    async fn count_active_sessions_with_mixed_statuses() {
        let mock = Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sess1": { "type": "idle" },
                "sess2": { "type": "busy" },
                "sess3": { "type": "retry", "attempt": 1, "message": "fail" }
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        assert_eq!(client.count_active_sessions().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn count_active_sessions_empty_map() {
        let mock = Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        assert_eq!(client.count_active_sessions().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn count_active_sessions_500_returns_error() {
        let mock = Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let result = client.count_active_sessions().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn count_active_sessions_sends_auth_header() {
        let mock = Mock::given(method("GET"))
            .and(path("/session/status"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        client.count_active_sessions().await.unwrap();
    }

    #[tokio::test]
    async fn test_get_session_statuses_returns_full_map() {
        let mock = Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sess1": { "type": "idle" },
                "sess2": { "type": "busy" },
                "sess3": { "type": "retry", "attempt": 1 }
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let statuses = client.get_session_statuses().await.unwrap();
        assert_eq!(statuses.len(), 3);
        assert!(statuses.contains_key("sess1"));
        assert!(statuses.contains_key("sess2"));
        assert!(statuses.contains_key("sess3"));
        // A completed session should NOT be in the map
        assert!(!statuses.contains_key("sess-done"));
    }

    #[tokio::test]
    async fn test_get_session_statuses_empty_map() {
        let mock = Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let statuses = client.get_session_statuses().await.unwrap();
        assert!(statuses.is_empty());
    }

    #[tokio::test]
    async fn test_get_session_statuses_500_returns_error() {
        let mock = Mock::given(method("GET"))
            .and(path("/session/status"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let result = client.get_session_statuses().await;
        assert!(result.is_err());
    }

    // --- get_session_messages (tests 20-24) ------------------------------

    #[tokio::test]
    async fn test_get_session_messages_returns_user_prompt() {
        let mock = Mock::given(method("GET"))
            .and(path("/session/sess1/message"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "info": {
                        "id": "msg1",
                        "role": "user",
                        "sessionID": "sess1",
                        "time": { "created": 1, "updated": 1 }
                    },
                    "parts": [{ "type": "text", "text": "issue body content" }]
                },
                {
                    "info": {
                        "id": "msg2",
                        "role": "assistant",
                        "sessionID": "sess1",
                        "time": { "created": 2, "updated": 2 }
                    },
                    "parts": [{ "type": "text", "text": "response" }]
                }
            ])))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let messages = client.get_session_messages("sess1", None).await.unwrap();

        assert_eq!(messages.len(), 2);
        let user_msg = messages.iter().find(|m| m.is_user()).unwrap();
        assert_eq!(user_msg.info.id, "msg1");
        assert_eq!(user_msg.text(), "issue body content");
        assert!(
            !messages
                .iter()
                .any(|m| !m.is_user() && m.text() == "issue body content")
        );
    }

    #[tokio::test]
    async fn test_get_session_messages_empty_array() {
        let mock = Mock::given(method("GET"))
            .and(path("/session/sess1/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let messages = client.get_session_messages("sess1", None).await.unwrap();
        assert!(messages.is_empty());
    }

    #[tokio::test]
    async fn test_get_session_messages_500_returns_error() {
        let mock = Mock::given(method("GET"))
            .and(path("/session/sess1/message"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let result = client.get_session_messages("sess1", None).await;
        assert!(matches!(
            result,
            Err(OpenCodeError::FetchSessionMessages(_))
        ));
    }

    #[tokio::test]
    async fn test_get_session_messages_with_directory_query_param() {
        let mock = Mock::given(method("GET"))
            .and(path("/session/sess1/message"))
            .and(query_param("directory", "/d"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        client
            .get_session_messages("sess1", Some("/d"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_session_message_text_concatenates_parts() {
        let body = json!([{
            "info": { "id": "m1", "role": "user", "sessionID": "s" },
            "parts": [
                { "type": "text", "text": "first" },
                { "type": "text", "text": "second" }
            ]
        }]);
        let msg: SessionMessage = serde_json::from_value(body[0].clone()).unwrap();
        assert_eq!(msg.text(), "first\nsecond");
        assert!(msg.is_user());
    }

    // --- create_workspace (tests 25-26) ---------------------------------

    #[tokio::test]
    async fn create_workspace_happy_path() {
        let mock = Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .and(query_param("directory", "/d"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .and(body_json(json!({ "type": "git" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "git",
                "name": "w1",
                "branch": null,
                "directory": null,
                "extra": null,
                "projectID": "p1",
                "timeUsed": 0
            })))
            .expect(1)
            .named("create_workspace");
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let ws = client.create_workspace("/d").await.unwrap();
        assert_eq!(ws.id, "wrk1");
        assert_eq!(ws.kind, "git");
        assert_eq!(ws.name, "w1");
        assert_eq!(ws.project_id, "p1");

        server.verify().await;
    }

    #[tokio::test]
    async fn create_workspace_500_returns_error() {
        let mock = Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let result = client.create_workspace("/d").await;
        assert!(matches!(result, Err(OpenCodeError::CreateWorkspace(_))));
    }

    // --- create_worktree (tests 27-28) ----------------------------------

    #[tokio::test]
    async fn create_worktree_happy_path() {
        let mock = Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .and(query_param("directory", "/d"))
            .and(query_param("workspace", "wrk1"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .and(body_json(json!({})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "name": "wt1",
                "branch": "issue-1",
                "directory": "/wt/dir1"
            })))
            .expect(1)
            .named("create_worktree");
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let wt = client.create_worktree("/d", "wrk1").await.unwrap();
        assert_eq!(wt.name, "wt1");
        assert_eq!(wt.branch.as_deref(), Some("issue-1"));
        assert_eq!(wt.directory, "/wt/dir1");

        server.verify().await;
    }

    #[tokio::test]
    async fn create_worktree_500_returns_error() {
        let mock = Mock::given(method("POST"))
            .and(path("/experimental/worktree"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let result = client.create_worktree("/d", "wrk1").await;
        assert!(matches!(result, Err(OpenCodeError::CreateWorktree(_))));
    }

    // --- start_session_with_system with workspace (tests 29-30) ----------

    #[tokio::test]
    async fn start_session_with_system_workspace_query_param() {
        let create = Mock::given(method("POST"))
            .and(path("/session"))
            .and(query_param("directory", "/d"))
            .and(query_param("workspace", "wrk1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .expect(1);
        let server = MockServer::start().await;
        create.mount(&server).await;

        let prompt = Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1);
        prompt.mount(&server).await;

        let client = client(&server);
        let id = client
            .start_session_with_system("/d", "t", "sys", "", "m", Some("wrk1"))
            .await
            .unwrap();
        assert_eq!(id, "sess123");
    }

    #[tokio::test]
    async fn start_session_with_system_none_workspace_omits_param() {
        // A mock that matches when a `workspace` query param is present must NOT match.
        let ws_present = Mock::given(method("POST"))
            .and(path("/session"))
            .and(query_param("workspace", "x"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "should-not-happen",
                "projectID": "p1",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .expect(0);
        let server = MockServer::start().await;
        ws_present.mount(&server).await;

        // Fallback: matches the no-workspace-param request.
        let ok = Mock::given(method("POST"))
            .and(path("/session"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "sess123",
                "projectID": "p1",
                "directory": "/d",
                "title": "t",
                "version": "1",
                "time": { "created": 1, "updated": 2 }
            })))
            .expect(1);
        ok.mount(&server).await;

        let prompt = Mock::given(method("POST"))
            .and(path("/session/sess123/prompt_async"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1);
        prompt.mount(&server).await;

        let client = client(&server);
        let id = client
            .start_session_with_system("/d", "t", "sys", "", "m", None)
            .await
            .unwrap();
        assert_eq!(id, "sess123");
    }
}
