use base64::Engine;
use reqwest::Client;
use serde_json::json;
use thiserror::Error;

use crate::external_agent::opencode::types::{
    ActiveSessionEntry, AgentV2, Data, LocationInfo, ModelRef, PromptReceipt, ServerInfo, Session,
    SessionMessage, SessionMessageV2, SessionMessagesResponse, SessionV2Info, Workspace, Worktree,
    WorktreeInfo,
};

/// Response wrapper for `GET /api/agent` — `{ "location": ..., "data": [...] }`.
///
/// `location` echoes the queried working directory and is not needed by
/// callers, but it is part of the wire shape so it is modelled here.
#[derive(serde::Deserialize)]
struct AgentListResponse {
    #[serde(default)]
    #[allow(dead_code)]
    location: Option<serde_json::Value>,
    data: Vec<AgentV2>,
}

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
    #[error("Failed to fetch location: {0}")]
    FetchLocation(String),
    #[error("Failed to fetch active sessions: HTTP status {0}")]
    FetchActiveSessions(u16),
    #[error("Failed to get session: {0}")]
    GetSession(String),
    #[error("Failed to send prompt: {0}")]
    SendPrompt(String),
    #[error("Prompt rejected, session busy: {0}")]
    PromptConflict(String),
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

    /// `GET /api/info` — v2 health probe. Returns `true` only when the
    /// server answers with a 2xx status *and* a body that deserializes into
    /// [`ServerInfo`]. (v2 has no `/global/health` and no `healthy` flag.)
    ///
    /// On *any* failure (network error, non-2xx status, or malformed JSON)
    /// the method returns `false` rather than propagating the error.
    pub async fn check_health(&self) -> bool {
        self.check_health_inner().await.unwrap_or_default()
    }

    async fn check_health_inner(&self) -> Result<bool, OpenCodeError> {
        let response = self
            .client
            .get(format!("{}/api/info", self.base_url))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        if !response.status().is_success() {
            return Ok(false);
        }
        let _info: ServerInfo = response.json().await?;
        Ok(true)
    }

    /// `GET /api/agent` — list available v2 agents.
    ///
    /// When `directory` is `Some`, it is sent as the deepObject query
    /// parameter `location[directory]=<dir>`. The response wraps the agent
    /// list in `{ "location": ..., "data": [...] }`; only `data` is returned.
    pub async fn get_agents(&self, directory: Option<&str>) -> Result<Vec<AgentV2>, OpenCodeError> {
        let mut request = self
            .client
            .get(format!("{}/api/agent", self.base_url))
            .header("Authorization", &self.auth_header);
        if let Some(dir) = directory {
            request = request.query(&[("location[directory]", dir)]);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(OpenCodeError::HttpStatus(response.status().as_u16()));
        }
        let agents: AgentListResponse = response.json().await?;
        Ok(agents.data)
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
        model: Option<&str>,
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
            model,
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
    #[expect(clippy::too_many_arguments)]
    pub async fn start_session_with_system(
        &self,
        directory: &str,
        title: &str,
        system_prompt: &str,
        agent: &str,
        message: &str,
        workspace: Option<&str>,
        model: Option<&str>,
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
            model,
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

    /// `GET /api/session/active` + `GET /api/session/{id}` — per-model counts
    /// of *active* v2 sessions.
    ///
    /// For every active session id the v2 session is fetched; entries that
    /// return 404 (finished between the two calls) or carry no `model` are
    /// skipped. Genuine errors propagate.
    pub async fn get_session_models(
        &self,
    ) -> Result<std::collections::HashMap<String, usize>, OpenCodeError> {
        let active = self.get_active_sessions().await?;
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for id in active.keys() {
            if let Some(session) = self.get_session_v2(id).await?
                && let Some(ref model) = session.model
            {
                *counts.entry(model.as_key()).or_insert(0) += 1;
            }
        }
        Ok(counts)
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
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(OpenCodeError::FetchSessionMessages(format!(
                "HTTP status {}: {}",
                status, body
            )));
        }
        let messages: Vec<SessionMessage> = response.json().await?;
        Ok(messages)
    }

    /// `POST /experimental/workspace` — create a new workspace.
    ///
    /// Sends `{"type": "worktree", "branch": null}` as the body with `directory`
    /// as a query param. `"worktree"` is OpenCode's built-in git-worktree
    /// adapter name; `branch: null` uses the current/default branch.
    /// Returns the created [`Workspace`] on success.
    pub async fn create_workspace(&self, directory: &str) -> Result<Workspace, OpenCodeError> {
        let response = self
            .client
            .post(format!("{}/experimental/workspace", self.base_url))
            .query(&[("directory", directory)])
            .header("Authorization", &self.auth_header)
            .json(&json!({ "type": "worktree", "branch": null }))
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(OpenCodeError::CreateWorkspace(format!(
                "workspace creation HTTP status {}: {}",
                status, body
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
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(OpenCodeError::CreateWorktree(format!(
                "worktree creation HTTP status {}: {}",
                status, body
            )));
        }
        let worktree: Worktree = response.json().await?;
        Ok(worktree)
    }

    /// `GET /api/info` — v2 server information (bare response, no envelope).
    pub async fn server_info(&self) -> Result<ServerInfo, OpenCodeError> {
        let response = self
            .client
            .get(format!("{}/api/info", self.base_url))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(OpenCodeError::HttpStatus(response.status().as_u16()));
        }
        let info: ServerInfo = response.json().await?;
        Ok(info)
    }

    /// `GET /api/location` — v2 working directory + project for *directory*.
    ///
    /// The directory is passed as a deepObject query parameter
    /// (`location[directory]=<dir>`). Bare response, no envelope.
    pub async fn get_location(&self, directory: &str) -> Result<LocationInfo, OpenCodeError> {
        let response = self
            .client
            .get(format!("{}/api/location", self.base_url))
            .query(&[("location[directory]", directory)])
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(OpenCodeError::HttpStatus(response.status().as_u16()));
        }
        let info: LocationInfo = response.json().await?;
        Ok(info)
    }

    /// `POST /api/worktree` — create a v2 git worktree for *project_id*.
    ///
    /// Body is `{"projectID": <id>}` plus an optional `"branch"` key.
    /// Bare `{"directory": ...}` response, no envelope.
    pub async fn create_worktree_v2(
        &self,
        project_id: &str,
        branch: Option<&str>,
    ) -> Result<WorktreeInfo, OpenCodeError> {
        let mut body = json!({ "projectID": project_id });
        if let Some(b) = branch {
            body["branch"] = json!(b);
        }
        let response = self
            .client
            .post(format!("{}/api/worktree", self.base_url))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(OpenCodeError::CreateWorktree(format!("{status}: {body}")));
        }
        let worktree: WorktreeInfo = response.json().await?;
        Ok(worktree)
    }

    /// `POST /api/session` — create a v2 session.
    ///
    /// Flat body `{"title", "agent", "location": {"directory"}}` with an
    /// optional nested `"model"` object; no query parameters. The response is
    /// enveloped in `{"data": ...}`.
    pub async fn create_session(
        &self,
        title: &str,
        agent: &str,
        model: Option<&ModelRef>,
        directory: &str,
    ) -> Result<SessionV2Info, OpenCodeError> {
        let mut body = json!({
            "title": title,
            "agent": agent,
            "location": { "directory": directory }
        });
        if let Some(m) = model {
            let mut model_json = json!({
                "id": m.id.as_str(),
                "providerID": m.provider_id.as_str()
            });
            if let Some(variant) = m.variant.as_deref() {
                model_json["variant"] = json!(variant);
            }
            body["model"] = model_json;
        }
        let response = self
            .client
            .post(format!("{}/api/session", self.base_url))
            .header("Authorization", &self.auth_header)
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(OpenCodeError::CreateSession(format!("{status}: {body}")));
        }
        let session: Data<SessionV2Info> = response.json().await?;
        if session.data.id.is_empty() {
            return Err(OpenCodeError::NoSessionData);
        }
        Ok(session.data)
    }

    /// `POST /api/session/{id}/prompt` — send a prompt to a v2 session.
    ///
    /// Body is exactly `{"text": <text>}`. The response is enveloped in
    /// `{"data": ...}`. 404 maps to [`OpenCodeError::HttpStatus`], 409 to
    /// [`OpenCodeError::PromptConflict`].
    pub async fn send_prompt(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<PromptReceipt, OpenCodeError> {
        let response = self
            .client
            .post(format!(
                "{}/api/session/{}/prompt",
                self.base_url, session_id
            ))
            .header("Authorization", &self.auth_header)
            .json(&json!({ "text": text }))
            .send()
            .await?;
        let status = response.status().as_u16();
        if status == 404 {
            return Err(OpenCodeError::HttpStatus(404));
        }
        if status == 409 {
            let body = response.text().await.unwrap_or_default();
            return Err(OpenCodeError::PromptConflict(body));
        }
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(OpenCodeError::SendPrompt(format!("{status}: {body}")));
        }
        let receipt: Data<PromptReceipt> = response.json().await?;
        Ok(receipt.data)
    }

    /// `GET /api/session/active` — map of active session IDs to their status
    /// entries, unwrapped from the `{"data": ...}` envelope.
    pub async fn get_active_sessions(
        &self,
    ) -> Result<std::collections::HashMap<String, ActiveSessionEntry>, OpenCodeError> {
        let response = self
            .client
            .get(format!("{}/api/session/active", self.base_url))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(OpenCodeError::FetchActiveSessions(
                response.status().as_u16(),
            ));
        }
        let active: Data<std::collections::HashMap<String, ActiveSessionEntry>> =
            response.json().await?;
        Ok(active.data)
    }

    /// `GET /api/session/{id}` — fetch a v2 session, unwrapped from the
    /// `{"data": ...}` envelope. 404 maps to `Ok(None)`.
    ///
    /// Named `get_session_v2` (not `get_session`) because an inherent
    /// `get_session` would shadow the `ExternalAgent::get_session` trait
    /// method on `OpenCodeClient` at every method-call site.
    pub async fn get_session_v2(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionV2Info>, OpenCodeError> {
        let response = self
            .client
            .get(format!("{}/api/session/{}", self.base_url, session_id))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        if response.status().as_u16() == 404 {
            return Ok(None);
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(OpenCodeError::GetSession(format!("{status}: {body}")));
        }
        let session: Data<SessionV2Info> = response.json().await?;
        Ok(Some(session.data))
    }

    /// `GET /api/session/{id}/message` — list v2 session messages, unwrapped
    /// from the `{"data": [...], "cursor": ...}` envelope (cursor ignored).
    pub async fn get_session_messages_v2(
        &self,
        session_id: &str,
    ) -> Result<Vec<SessionMessageV2>, OpenCodeError> {
        let response = self
            .client
            .get(format!(
                "{}/api/session/{}/message",
                self.base_url, session_id
            ))
            .header("Authorization", &self.auth_header)
            .send()
            .await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(OpenCodeError::FetchSessionMessages(format!(
                "{status}: {body}"
            )));
        }
        let messages: SessionMessagesResponse = response.json().await?;
        Ok(messages.data)
    }
}

/// Shared two-step HTTP flow used by [`OpenCodeClient::start_session`] and the
/// `ExternalAgent` trait impl to create a session and deliver a prompt.
#[expect(clippy::too_many_arguments)]
pub(crate) async fn start_session_http(
    client: &Client,
    base_url: &str,
    auth_header: &str,
    directory: &str,
    workspace: Option<&str>,
    title: &str,
    prompt_body: serde_json::Value,
    model: Option<&str>,
) -> Result<String, OpenCodeError> {
    let mut query = vec![("directory", directory)];
    if let Some(ws) = workspace {
        query.push(("workspace", ws));
    }
    let mut create_body = json!({ "title": title });
    if let Some(m) = model {
        create_body["model"] = json!(m);
    }
    let create_response = client
        .post(format!("{}/session", base_url))
        .query(&query)
        .header("Authorization", auth_header)
        .json(&create_body)
        .send()
        .await?;

    if !create_response.status().is_success() {
        let status = create_response.status().as_u16();
        let body = create_response.text().await.unwrap_or_default();
        return Err(OpenCodeError::CreateSession(format!(
            "session creation HTTP status {}: {}",
            status, body
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
        let status = prompt_response.status().as_u16();
        let body = prompt_response.text().await.unwrap_or_default();
        return Err(OpenCodeError::CreateSession(format!(
            "prompt_async HTTP status {}: {}",
            status, body
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

    /// TDD gate: v2 health check must hit `GET /api/info` and never the v1
    /// `GET /global/health` endpoint.
    #[tokio::test]
    async fn check_health_uses_api_info_not_global_health() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": "2.0.18",
                "pid": 47234,
                "urls": ["http://127.0.0.1:4096"],
                "paths": { "tmp": "/tmp/opencode" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "healthy": true, "version": "1.0.0" })),
            )
            .expect(0)
            .mount(&server)
            .await;

        let client = client(&server);
        assert!(client.check_health().await);

        server.verify().await;
    }

    #[tokio::test]
    async fn test_check_health_healthy() {
        let mock = Mock::given(method("GET"))
            .and(path("/api/info"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": "2.0.18",
                "pid": 47234,
                "urls": ["http://127.0.0.1:4096"],
                "paths": { "tmp": "/tmp/opencode" }
            })))
            .expect(1)
            .named("health");
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        assert!(client.check_health().await);
    }

    #[tokio::test]
    async fn test_check_health_incomplete_server_info_returns_false() {
        // v2 has no `healthy` flag; a 2xx body that does not deserialize into
        // `ServerInfo` (here: missing pid/urls/paths) must yield `false`.
        let mock = Mock::given(method("GET"))
            .and(path("/api/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": "2.0.18" })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        assert!(!client.check_health().await);
    }

    #[tokio::test]
    async fn test_check_health_500_returns_false() {
        let mock = Mock::given(method("GET"))
            .and(path("/api/info"))
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
            .and(path("/api/info"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("not json", "text/plain"))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        assert!(!client.check_health().await);
    }

    #[tokio::test]
    async fn test_check_health_missing_paths_field_returns_false() {
        let mock = Mock::given(method("GET"))
            .and(path("/api/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": "2.0.18",
                "pid": 47234,
                "urls": ["http://127.0.0.1:4096"]
            })))
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
            .and(path("/api/agent"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "location": {},
                "data": [
                    { "id": "agent1", "name": "agent1", "mode": "subagent", "hidden": false }
                ]
            })))
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
            .and(path("/api/agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "location": {},
                "data": [
                    {
                        "id": "a",
                        "name": "a",
                        "mode": "primary",
                        "hidden": false,
                        "description": "test"
                    }
                ]
            })))
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
    async fn test_get_agents_unwraps_location_data_envelope() {
        // The v2 response wraps agents in { "location": ..., "data": [...] };
        // a populated `location` object must not break parsing.
        let mock = Mock::given(method("GET"))
            .and(path("/api/agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "location": {
                    "directory": "/d",
                    "project": { "id": "p1", "directory": "/d", "canonical": "/d" }
                },
                "data": [
                    { "id": "build", "name": "build", "mode": "primary", "hidden": false }
                ]
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let agents = client.get_agents(None).await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].id, "build");
    }

    #[tokio::test]
    async fn test_get_agents_with_directory_query_param() {
        let mock = Mock::given(method("GET"))
            .and(path("/api/agent"))
            .and(query_param("location[directory]", "/path"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "location": {}, "data": [] })),
            )
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let agents = client.get_agents(Some("/path")).await.unwrap();
        assert!(agents.is_empty());
    }

    #[tokio::test]
    async fn test_get_agents_no_directory_omits_query_param() {
        // If a `location[directory]` param is present the request must NOT
        // match this mock (wiremock returns 404 for unmatched requests,
        // surfacing a real error).
        let mock = Mock::given(method("GET"))
            .and(path("/api/agent"))
            .and(query_param("location[directory]", "/absent"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "location": {}, "data": [] })),
            )
            .expect(0);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        // Mount a fallback that matches the no-query-param request.
        let ok = Mock::given(method("GET"))
            .and(path("/api/agent"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "location": {}, "data": [] })),
            )
            .expect(1);
        ok.mount(&server).await;

        let client = client(&server);
        client.get_agents(None).await.unwrap();
    }

    #[tokio::test]
    async fn test_get_agents_500_returns_http_status_error() {
        let mock = Mock::given(method("GET"))
            .and(path("/api/agent"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let result = client.get_agents(None).await;
        assert!(matches!(result, Err(OpenCodeError::HttpStatus(500))));
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
            .start_session(
                "/d",
                "Session Title",
                "git-automate-triage",
                "issue body",
                None,
            )
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
            .start_session("/d", "t", "git-automate-triage", "m", None)
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
            .start_session(
                "/d",
                "Session Title",
                "git-automate-triage",
                "issue body",
                None,
            )
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
            .start_session(
                "/d",
                "Session Title",
                "git-automate-triage",
                "issue body",
                None,
            )
            .await
            .unwrap();
    }

    // --- auth header on all requests (test 15) --------------------------

    #[tokio::test]
    async fn test_auth_header_sent_on_all_requests() {
        // health
        let health = Mock::given(method("GET"))
            .and(path("/api/info"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": "2.0.18",
                "pid": 47234,
                "urls": ["http://127.0.0.1:4096"],
                "paths": { "tmp": "/tmp/opencode" }
            })))
            .expect(1)
            .named("health");
        // agent
        let agent = Mock::given(method("GET"))
            .and(path("/api/agent"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "location": {}, "data": [] })),
            )
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
            .start_session("/d", "t", "git-automate-triage", "m", None)
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
            .and(body_json(json!({ "type": "worktree", "branch": null })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "wrk1",
                "type": "worktree",
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
        assert_eq!(ws.kind, "worktree");
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

    #[tokio::test]
    async fn create_workspace_400_includes_response_body() {
        let mock = Mock::given(method("POST"))
            .and(path("/experimental/workspace"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "name": "WorkspaceCreateError",
                "data": { "message": "Adapter 'git' not found" }
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let result = client.create_workspace("/d").await;
        match result {
            Err(OpenCodeError::CreateWorkspace(msg)) => {
                assert!(
                    msg.contains("Adapter 'git' not found"),
                    "error message should contain response body, got: {msg}"
                );
            }
            other => panic!("expected CreateWorkspace error, got {other:?}"),
        }
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
            .start_session_with_system("/d", "t", "sys", "", "m", Some("wrk1"), None)
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
            .start_session_with_system("/d", "t", "sys", "", "m", None, None)
            .await
            .unwrap();
        assert_eq!(id, "sess123");
    }

    // --- start_session_with_system model (tests 31-32) -----------------

    #[tokio::test]
    async fn start_session_with_system_includes_model_in_body() {
        let create = Mock::given(method("POST"))
            .and(path("/session"))
            .and(body_json(
                json!({ "title": "t", "model": "myprovider/fast" }),
            ))
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
            .start_session_with_system("/d", "t", "sys", "", "m", None, Some("myprovider/fast"))
            .await
            .unwrap();
        assert_eq!(id, "sess123");
    }

    #[tokio::test]
    async fn start_session_with_system_omits_model_when_none() {
        let create = Mock::given(method("POST"))
            .and(path("/session"))
            .and(body_json(json!({ "title": "t" })))
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
            .start_session_with_system("/d", "t", "sys", "", "m", None, None)
            .await
            .unwrap();
        assert_eq!(id, "sess123");
    }

    // --- get_session_models (tests 33-36) --------------------------

    #[tokio::test]
    async fn get_session_models_happy_path() {
        let server = MockServer::start().await;

        // GET /api/session/active — ses_1 and ses_2 are running; ses_3 is not.
        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "ses_1": { "type": "running" },
                    "ses_2": { "type": "running" }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/ses_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses_1", "model": { "id": "fast", "providerID": "myprovider" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/ses_2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses_2", "model": { "id": "fast", "providerID": "myprovider" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let counts = client.get_session_models().await.unwrap();
        assert_eq!(counts.get("myprovider/fast"), Some(&2));
        // Only active sessions are fetched, so no other model appears.
        assert_eq!(counts.len(), 1);

        server.verify().await;
    }

    #[tokio::test]
    async fn get_session_models_no_active_sessions() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let counts = client.get_session_models().await.unwrap();
        assert!(counts.is_empty());

        server.verify().await;
    }

    #[tokio::test]
    async fn get_session_models_excludes_sessions_without_model() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "ses_1": { "type": "running" },
                    "ses_2": { "type": "running" }
                }
            })))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/ses_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses_1" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/ses_2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses_2", "model": { "id": "fast", "providerID": "myprovider" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let counts = client.get_session_models().await.unwrap();
        // ses_1 has no model, so it's excluded
        assert_eq!(counts.len(), 1);
        assert_eq!(counts.get("myprovider/fast"), Some(&1));

        server.verify().await;
    }

    #[tokio::test]
    async fn get_session_models_skips_sessions_that_404() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "ses_gone": { "type": "running" },
                    "ses_2": { "type": "running" }
                }
            })))
            .mount(&server)
            .await;

        // ses_gone finished between the /active listing and the per-id fetch.
        Mock::given(method("GET"))
            .and(path("/api/session/ses_gone"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/session/ses_2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses_2", "model": { "id": "slow", "providerID": "myprovider" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let counts = client.get_session_models().await.unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts.get("myprovider/slow"), Some(&1));

        server.verify().await;
    }

    #[tokio::test]
    async fn get_session_models_propagates_active_sessions_error() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let result = client.get_session_models().await;
        assert!(matches!(
            result,
            Err(OpenCodeError::FetchActiveSessions(500))
        ));

        server.verify().await;
    }

    // ─── v2 API methods (tests 37-47) ───────────────────────────────

    /// Matches only when the request body is *exactly* the expected JSON.
    ///
    /// wiremock's built-in `body_json` matcher is a subset match, so it
    /// cannot prove the *absence* of extra keys (e.g. `parts`, `system`,
    /// `agent` on the v2 prompt body).
    struct ExactJsonBody(serde_json::Value);

    impl wiremock::Match for ExactJsonBody {
        fn matches(&self, request: &wiremock::Request) -> bool {
            request
                .body_json::<serde_json::Value>()
                .is_ok_and(|actual| actual == self.0)
        }
    }

    /// Matches only when the request URL carries no query parameters at all.
    struct NoQueryParams;

    impl wiremock::Match for NoQueryParams {
        fn matches(&self, request: &wiremock::Request) -> bool {
            request.url.query().is_none_or(|q| q.is_empty())
        }
    }

    // T37: GET /api/info returns a bare ServerInfo with auth header.
    #[tokio::test]
    async fn server_info_hits_api_info() {
        let mock = Mock::given(method("GET"))
            .and(path("/api/info"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "version": "2.0.0",
                "pid": 1234,
                "urls": ["http://localhost:8081"],
                "paths": { "tmp": "/tmp/opencode" }
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let info = client.server_info().await.unwrap();
        assert_eq!(info.version, "2.0.0");
        assert_eq!(info.pid, 1234);
        assert_eq!(info.urls, vec!["http://localhost:8081".to_string()]);
        assert_eq!(info.paths.tmp, "/tmp/opencode");

        server.verify().await;
    }

    // T38: GET /api/location uses the deepObject query `location[directory]`.
    #[tokio::test]
    async fn get_location_uses_deepobject_query() {
        let mock = Mock::given(method("GET"))
            .and(path("/api/location"))
            .and(query_param("location[directory]", "/wt"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt",
                "project": { "id": "p1", "directory": "/wt", "canonical": "/wt" }
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let info = client.get_location("/wt").await.unwrap();
        assert_eq!(info.directory, "/wt");
        assert_eq!(info.project.id, "p1");
        assert_eq!(info.project.canonical, "/wt");

        server.verify().await;
    }

    // T39: POST /api/session sends a flat body with NO query params and
    // unwraps the {"data": ...} envelope.
    #[tokio::test]
    async fn create_session_posts_flat_body_and_unwraps_data() {
        let mock = Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .and(ExactJsonBody(json!({
                "title": "t",
                "agent": "git-automate-triage",
                "location": { "directory": "/wt/dir1" }
            })))
            .and(NoQueryParams)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "ses_1" }
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let info = client
            .create_session("t", "git-automate-triage", None, "/wt/dir1")
            .await
            .unwrap();
        assert_eq!(info.id, "ses_1");

        server.verify().await;
    }

    // T40: POST /api/session serialises the model as a nested object.
    #[tokio::test]
    async fn create_session_with_model_includes_model_object() {
        let mock = Mock::given(method("POST"))
            .and(path("/api/session"))
            .and(ExactJsonBody(json!({
                "title": "t",
                "agent": "git-automate-triage",
                "location": { "directory": "/wt/dir1" },
                "model": { "id": "fast", "providerID": "myprovider", "variant": "v1" }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "id": "ses_2",
                    "model": { "id": "fast", "providerID": "myprovider", "variant": "v1" }
                }
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let model = ModelRef {
            id: "fast".into(),
            provider_id: "myprovider".into(),
            variant: Some("v1".into()),
        };
        let client = client(&server);
        let info = client
            .create_session("t", "git-automate-triage", Some(&model), "/wt/dir1")
            .await
            .unwrap();
        assert_eq!(info.id, "ses_2");
        let returned = info.model.unwrap();
        assert_eq!(returned.id, "fast");
        assert_eq!(returned.provider_id, "myprovider");
        assert_eq!(returned.variant.as_deref(), Some("v1"));

        server.verify().await;
    }

    // T41: POST /api/session/{id}/prompt sends ONLY {"text": ...} (no
    // `parts`/`system`/`agent` keys) and unwraps the receipt envelope.
    #[tokio::test]
    async fn send_prompt_posts_flat_text_and_returns_receipt() {
        let mock = Mock::given(method("POST"))
            .and(path("/api/session/ses_1/prompt"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .and(ExactJsonBody(json!({ "text": "hello" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "id": "msg_1" }
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let receipt = client.send_prompt("ses_1", "hello").await.unwrap();
        assert_eq!(receipt.id, "msg_1");

        server.verify().await;
    }

    // T42: POST /api/session/{id}/prompt 409 maps to PromptConflict.
    #[tokio::test]
    async fn send_prompt_409_is_prompt_conflict() {
        let mock = Mock::given(method("POST"))
            .and(path("/api/session/ses_1/prompt"))
            .respond_with(ResponseTemplate::new(409).set_body_string("session is busy"))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let result = client.send_prompt("ses_1", "hello").await;
        match result {
            Err(OpenCodeError::PromptConflict(body)) => {
                assert!(body.contains("session is busy"), "got: {body}");
            }
            other => panic!("expected PromptConflict, got {other:?}"),
        }

        server.verify().await;
    }

    // T43: GET /api/session/active unwraps the data envelope into a map.
    #[tokio::test]
    async fn get_active_sessions_unwraps_data() {
        let mock = Mock::given(method("GET"))
            .and(path("/api/session/active"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "ses_1": { "type": "running" }
                }
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let active = client.get_active_sessions().await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active["ses_1"].kind, "running");

        server.verify().await;
    }

    // T44: GET /api/session/{id} 404 returns Ok(None).
    #[tokio::test]
    async fn get_session_404_returns_none() {
        let mock = Mock::given(method("GET"))
            .and(path("/api/session/missing"))
            .respond_with(ResponseTemplate::new(404))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let result = client.get_session_v2("missing").await.unwrap();
        assert!(result.is_none());

        server.verify().await;
    }

    // T45: GET /api/session/{id} unwraps the data envelope.
    #[tokio::test]
    async fn get_session_unwraps_data() {
        let mock = Mock::given(method("GET"))
            .and(path("/api/session/ses_1"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "id": "ses_1",
                    "title": "T",
                    "projectID": "p1",
                    "location": { "directory": "/wt" }
                }
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let info = client.get_session_v2("ses_1").await.unwrap().unwrap();
        assert_eq!(info.id, "ses_1");
        assert_eq!(info.title.as_deref(), Some("T"));
        assert_eq!(info.project_id.as_deref(), Some("p1"));

        server.verify().await;
    }

    // T46: GET /api/session/{id}/message unwraps the data envelope.
    #[tokio::test]
    async fn get_session_messages_v2_unwraps_data() {
        let mock = Mock::given(method("GET"))
            .and(path("/api/session/ses_1/message"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [
                    { "id": "m1", "type": "user", "text": "hi" }
                ]
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let messages = client.get_session_messages_v2("ses_1").await.unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_user());
        assert_eq!(messages[0].text(), "hi");

        server.verify().await;
    }

    // T47: POST /api/worktree posts the project id and returns a bare
    // {"directory": ...} response.
    #[tokio::test]
    async fn create_worktree_v2_posts_project_id() {
        let mock = Mock::given(method("POST"))
            .and(path("/api/worktree"))
            .and(header("Authorization", "Basic b3BlbmNvZGU6cHc="))
            .and(body_json(json!({ "projectID": "p1" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "directory": "/wt/x"
            })))
            .expect(1);
        let server = MockServer::start().await;
        mock.mount(&server).await;

        let client = client(&server);
        let wt = client.create_worktree_v2("p1", None).await.unwrap();
        assert_eq!(wt.directory, "/wt/x");

        server.verify().await;
    }
}
