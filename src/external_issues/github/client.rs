//! GitHub API client supporting both GraphQL (Projects V2) and REST operations.
//! Authentication uses a personal access token. GraphQL requests go to
//! `https://api.github.com/graphql`; REST requests go to `https://api.github.com/`.

use super::types::*;
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use thiserror::Error;

// ─── Error type ───────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("GitHub token is required")]
    EmptyToken,
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP status {0}")]
    HttpStatus(u16),
    #[error("GraphQL error: {0}")]
    GraphQLError(String),
    #[error("Project not found: {0}")]
    ProjectNotFound(String),
    #[error("Could not resolve owner node ID for: {0}")]
    OwnerNotFound(String),
    #[error("{0}")]
    Other(String),
}

impl From<serde_json::Error> for GitHubError {
    fn from(e: serde_json::Error) -> Self {
        GitHubError::Other(e.to_string())
    }
}

// ─── Client ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GitHubClient {
    client: Client,
    token: String,
    base_url: String,
}

impl GitHubClient {
    /// Create a new client targeting the real GitHub API.
    ///
    /// # Errors
    /// Returns `GitHubError::EmptyToken` if `token` is empty.
    pub fn new(token: String) -> Result<Self, GitHubError> {
        if token.is_empty() {
            return Err(GitHubError::EmptyToken);
        }
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| GitHubError::Other(e.to_string()))?;
        Ok(Self {
            client,
            token,
            base_url: "https://api.github.com".to_string(),
        })
    }

    /// Create a client targeting a custom base URL (for testing/integration).
    pub fn new_with_base_url(token: String, base_url: String) -> Result<Self, GitHubError> {
        if token.is_empty() {
            return Err(GitHubError::EmptyToken);
        }
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| GitHubError::Other(e.to_string()))?;
        Ok(Self {
            client,
            token,
            base_url,
        })
    }

    // ── Core transport ──────────────────────────────────────────

    fn authenticated_request(
        &self,
        method: reqwest::Method,
        url: String,
    ) -> reqwest::RequestBuilder {
        self.client
            .request(method, url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("User-Agent", "git-automate")
            .header("Accept", "application/vnd.github+json")
    }

    /// Execute a GraphQL query or mutation.
    ///
    /// POSTs to `{base_url}/graphql` with body `{ query, variables }`.
    /// On non-200: returns `Err(HttpStatus(status))`.
    /// On `{"errors": ...}`: returns `Err(GraphQLError(...))`.
    /// On `{"data": null}` or missing `data`: returns `Err(GraphQLError(...))`.
    /// Otherwise deserializes the `data` field into `T`.
    pub async fn graphql<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: Option<&Value>,
    ) -> Result<T, GitHubError> {
        let vars = variables.cloned().unwrap_or_default();
        let payload = json!({
            "query": query,
            "variables": vars,
        });

        let request = self
            .authenticated_request(reqwest::Method::POST, format!("{}/graphql", self.base_url))
            .json(&payload);

        let response = request.send().await?;

        let status = response.status().as_u16();
        if status != 200 {
            return Err(GitHubError::HttpStatus(status));
        }

        let body: Value = response.json().await?;

        if let Some(errors) = body.get("errors") {
            return Err(GitHubError::GraphQLError(errors.to_string()));
        }

        let data = body.get("data").ok_or_else(|| {
            GitHubError::GraphQLError("response missing 'data' field".to_string())
        })?;

        if data.is_null() {
            return Err(GitHubError::GraphQLError(
                "'data' field is null".to_string(),
            ));
        }

        let result: T = serde_json::from_value(data.clone())?;
        Ok(result)
    }

    /// GET `{base_url}{path}?{query}` (query is optional).
    /// Returns the parsed JSON body as a `Value`.
    /// On non-2xx: returns `Err(HttpStatus(status))`.
    async fn rest_get(&self, path: &str, query: Option<&str>) -> Result<Value, GitHubError> {
        let url = match query {
            Some(q) => format!("{}{}?{}", self.base_url, path, q),
            None => format!("{}{}", self.base_url, path),
        };

        let response = self
            .authenticated_request(reqwest::Method::GET, url)
            .send()
            .await?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(GitHubError::HttpStatus(status));
        }

        let body: Value = response.json().await?;
        Ok(body)
    }

    /// POST `{base_url}{path}` with a JSON body.
    /// Returns the parsed JSON body as a `Value`.
    /// On non-2xx: returns `Err(HttpStatus(status))`.
    async fn rest_post(&self, path: &str, body: &Value) -> Result<Value, GitHubError> {
        let url = format!("{}{}", self.base_url, path);

        let response = self
            .authenticated_request(reqwest::Method::POST, url)
            .json(body)
            .send()
            .await?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(GitHubError::HttpStatus(status));
        }

        let resp_body: Value = response.json().await?;
        Ok(resp_body)
    }

    // ── Project V2 methods ─────────────────────────────────────

    /// Resolve a login to a node ID by trying both user and organization.
    async fn get_owner_id(&self, login: &str) -> Result<String, GitHubError> {
        let result = self
            .graphql::<NodeOwnerResult>(
                r#"query($login: String!) {
                    user(login: $login) { id }
                    organization(login: $login) { id }
                }"#,
                Some(&json!({ "login": login })),
            )
            .await?;

        let owner_id = result
            .user
            .map(|u| u.id)
            .or_else(|| result.organization.map(|o| o.id));

        match owner_id {
            Some(id) => Ok(id),
            None => Err(GitHubError::OwnerNotFound(login.to_string())),
        }
    }

    /// Create a GitHub Project V2 and return the project node ID.
    pub async fn create_project(&self, owner: &str, title: &str) -> Result<String, GitHubError> {
        let owner_id = self.get_owner_id(owner).await?;

        let result = self
            .graphql::<CreateProjectV2Result>(
                r#"mutation($input: CreateProjectV2Input!) {
                    createProjectV2(input: $input) {
                        id
                    }
                }"#,
                Some(&json!({ "input": { "title": title, "ownerId": owner_id } })),
            )
            .await?;

        Ok(result.create_project_v2.id)
    }

    /// Fetch a Project V2 by node ID.
    pub async fn get_project(&self, project_id: &str) -> Result<ProjectV2Summary, GitHubError> {
        let result = self
            .graphql::<NodeProjectResult>(
                r#"query($id: ID!) {
                    node(id: $id) {
                        ... on ProjectV2 {
                            id
                            number
                            title
                        }
                    }
                }"#,
                Some(&json!({ "id": project_id })),
            )
            .await?;

        let node = result
            .node
            .ok_or_else(|| GitHubError::ProjectNotFound(project_id.to_string()))?;

        Ok(ProjectV2Summary {
            id: node.id,
            number: node.number.to_string(),
            title: node.title,
        })
    }

    /// List all fields on a Project V2.
    pub async fn get_project_fields(
        &self,
        project_id: &str,
    ) -> Result<Vec<ProjectFieldInfo>, GitHubError> {
        let result = self
            .graphql::<NodeFieldsResult>(
                r#"query($id: ID!) {
                    node(id: $id) {
                        ... on ProjectV2 {
                            fields(first: 100) {
                                nodes {
                                    id
                                    name
                                    dataType
                                }
                            }
                        }
                    }
                }"#,
                Some(&json!({ "id": project_id })),
            )
            .await?;

        let node = result
            .node
            .ok_or_else(|| GitHubError::ProjectNotFound(project_id.to_string()))?;

        Ok(node.fields.nodes)
    }

    /// Create a new field on a Project V2 and return the field node ID.
    pub async fn add_project_field(
        &self,
        project_id: &str,
        name: &str,
        data_type: &str,
    ) -> Result<String, GitHubError> {
        let result = self
            .graphql::<CreateFieldResult>(
                r#"mutation($input: CreateProjectV2FieldInput!) {
                    createProjectV2Field(input: $input) {
                        projectField {
                            id
                        }
                    }
                }"#,
                Some(&json!({
                    "input": {
                        "projectId": project_id,
                        "name": name,
                        "dataType": data_type,
                    }
                })),
            )
            .await?;

        Ok(result.create_project_v2_field.project_field.id)
    }

    /// Find the project's "Status" single-select field and its options.
    ///
    /// Returns `Ok(None)` if the field doesn't exist or isn't a
    /// single-select field.
    pub async fn get_project_status_field(
        &self,
        project_id: &str,
    ) -> Result<Option<StatusFieldInfo>, GitHubError> {
        let result = self
            .graphql::<StatusFieldResult>(
                r#"query($id: ID!, $name: String!) {
                    node(id: $id) {
                        ... on ProjectV2 {
                            field(name: $name) {
                                ... on ProjectV2SingleSelectField {
                                    id
                                    options {
                                        id
                                        name
                                    }
                                }
                            }
                        }
                    }
                }"#,
                Some(&json!({ "id": project_id, "name": "Status" })),
            )
            .await?;

        let field = match result.node.and_then(|n| n.field) {
            Some(f) => f,
            None => return Ok(None),
        };

        Ok(Some(StatusFieldInfo {
            id: field.id,
            options: field.options,
        }))
    }

    /// Add new options to a single-select field's configuration.
    pub async fn add_project_status_options(
        &self,
        field_id: &str,
        options: &[&str],
    ) -> Result<(), GitHubError> {
        let add_options: Vec<Value> = options.iter().map(|name| json!({ "name": name })).collect();

        self.graphql::<UpdateFieldConfigResult>(
            r#"mutation($input: UpdateProjectV2FieldConfigurationInput!) {
                updateProjectV2FieldConfiguration(input: $input) {
                    projectV2Field {
                        id
                    }
                }
            }"#,
            Some(&json!({
                "input": {
                    "fieldId": field_id,
                    "addOptions": add_options,
                }
            })),
        )
        .await?;

        Ok(())
    }

    /// List all items in a Project V2 with their content reference.
    ///
    /// Items whose `content` is `null` are filtered out.
    pub async fn list_project_items(
        &self,
        project_id: &str,
    ) -> Result<Vec<ProjectItem>, GitHubError> {
        let result = self
            .graphql::<ListProjectItemsResult>(
                r#"query($id: ID!) {
                    node(id: $id) {
                        ... on ProjectV2 {
                            items(first: 100) {
                                nodes {
                                    id
                                    content {
                                        __typename
                                        id
                                        ... on Issue {
                                            number
                                        }
                                        ... on PullRequest {
                                            number
                                        }
                                    }
                                }
                            }
                        }
                    }
                }"#,
                Some(&json!({ "id": project_id })),
            )
            .await?;

        let Some(node) = result.node else {
            return Ok(vec![]);
        };

        let items = node
            .items
            .nodes
            .into_iter()
            .filter(|item| item.content.is_some())
            .filter_map(|item| {
                let content = item.content?;
                Some(ProjectItem {
                    id: item.id,
                    content_node_id: content.id,
                    content_type: content.typename,
                    content_number: content.number,
                })
            })
            .collect();

        Ok(items)
    }

    /// Set a single-select (status) field value on a project item.
    ///
    /// `optionId` must be the option ID (not the option name).
    pub async fn update_project_item_status(
        &self,
        project_id: &str,
        item_id: &str,
        field_id: &str,
        option_id: &str,
    ) -> Result<(), GitHubError> {
        self.graphql::<UpdateItemFieldValueResult>(
            r#"mutation($input: UpdateProjectV2ItemFieldValueInput!) {
                updateProjectV2ItemFieldValue(input: $input) {
                    projectV2Item {
                        id
                    }
                }
            }"#,
            Some(&json!({
                "input": {
                    "projectId": project_id,
                    "itemId": item_id,
                    "fieldId": field_id,
                    "value": { "optionId": option_id },
                }
            })),
        )
        .await?;

        Ok(())
    }

    /// Set a text field (e.g. session ID) value on a project item.
    pub async fn update_project_item_session_id(
        &self,
        project_id: &str,
        item_id: &str,
        field_id: &str,
        session_id: &str,
    ) -> Result<(), GitHubError> {
        self.graphql::<UpdateItemFieldValueResult>(
            r#"mutation($input: UpdateProjectV2ItemFieldValueInput!) {
                updateProjectV2ItemFieldValue(input: $input) {
                    projectV2Item {
                        id
                    }
                }
            }"#,
            Some(&json!({
                "input": {
                    "projectId": project_id,
                    "itemId": item_id,
                    "fieldId": field_id,
                    "value": { "text": session_id },
                }
            })),
        )
        .await?;

        Ok(())
    }

    /// Add an existing issue (by content node ID) to a Project V2.
    pub async fn add_issue_to_project(
        &self,
        content_id: &str,
        project_id: &str,
    ) -> Result<String, GitHubError> {
        let result = self
            .graphql::<AddItemResult>(
                r#"mutation($input: AddProjectV2ItemByIdInput!) {
                    addProjectV2ItemById(input: $input) {
                        item {
                            id
                        }
                    }
                }"#,
                Some(&json!({
                    "input": {
                        "contentId": content_id,
                        "projectId": project_id,
                    }
                })),
            )
            .await?;

        Ok(result.add_project_v2_item_by_id.item.id)
    }

    /// Fetch all field values for a single project item, keyed by field name.
    ///
    /// For each field, `text` takes priority over `option`.
    pub async fn get_project_item_values(
        &self,
        item_id: &str,
    ) -> Result<BTreeMap<String, Option<String>>, GitHubError> {
        let result = self
            .graphql::<NodeFieldValuesResult>(
                r#"query($id: ID!) {
                    node(id: $id) {
                        ... on ProjectV2Item {
                            fieldValues(first: 100) {
                                nodes {
                                    name
                                    ... on ProjectV2ItemFieldTextValue {
                                        text
                                    }
                                    ... on ProjectV2ItemFieldSingleSelectValue {
                                        option
                                    }
                                }
                            }
                        }
                    }
                }"#,
                Some(&json!({ "id": item_id })),
            )
            .await?;

        let mut values: BTreeMap<String, Option<String>> = BTreeMap::new();

        if let Some(node) = result.node {
            for field in node.field_values.nodes {
                let Some(name) = field.name else { continue };
                let value = field.text.or(field.option);
                values.insert(name, value);
            }
        }

        Ok(values)
    }

    // ── Issue methods (REST) ───────────────────────────────────

    /// List all issues (open and closed) in a repository via REST.
    ///
    /// Pure issues are returned; pull requests are filtered out.
    pub async fn list_repo_issues(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<IssueInfo>, GitHubError> {
        let path = format!("/repos/{}/{}/issues", owner, repo);
        let data = self.rest_get(&path, None).await?;

        let issues: Vec<RestIssue> = serde_json::from_value(data)?;

        let result = issues
            .into_iter()
            .filter(|issue| issue.pull_request.is_none())
            .map(|issue| IssueInfo {
                id: issue.node_id,
                number: issue.number,
                title: issue.title,
                body: issue.body,
                state: issue.state,
            })
            .collect();

        Ok(result)
    }

    /// Search issues in a repository via the REST search API.
    pub async fn search_issues(
        &self,
        owner: &str,
        repo: &str,
        query: &str,
    ) -> Result<Vec<IssueInfo>, GitHubError> {
        let path = "/search/issues";
        let q = format!("repo:{}/{} {}", owner, repo, query);

        let data = self.rest_get(path, Some(&q)).await?;

        let rest_result: serde_json::Value = data;
        let items = rest_result.get("items").ok_or_else(|| {
            GitHubError::Other("search response missing 'items' field".to_string())
        })?;

        let issues: Vec<RestIssue> = serde_json::from_value(items.clone())?;

        let result = issues
            .into_iter()
            .map(|issue| IssueInfo {
                id: issue.node_id,
                number: issue.number,
                title: issue.title,
                body: issue.body,
                state: issue.state,
            })
            .collect();

        Ok(result)
    }

    /// Get the GraphQL node ID for a REST issue.
    pub async fn get_issue_node_id(
        &self,
        owner: &str,
        repo: &str,
        issue_number: i64,
    ) -> Result<String, GitHubError> {
        let path = format!("/repos/{}/{}/issues/{}", owner, repo, issue_number);
        let data = self.rest_get(&path, None).await?;

        let result: RestIssueNode = serde_json::from_value(data)?;
        Ok(result.node_id)
    }

    // ── Issue hierarchy (GraphQL) ──────────────────────────────

    /// List all issues in a repository with their parent issue number (if any),
    /// using the GraphQL API.
    ///
    /// This can be used to build a parent → children (sub-task) map.
    pub async fn list_issues_with_parents(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<IssueWithParent>, GitHubError> {
        let result = self
            .graphql::<IssuesWithParentsResult>(
                r#"query($owner: String!, $repo: String!) {
                    repository(owner: $owner, name: $repo) {
                        issues(first: 100) {
                            nodes {
                                id
                                number
                                title
                                body
                                state
                                parentIssue {
                                    number
                                }
                            }
                        }
                    }
                }"#,
                Some(&json!({ "owner": owner, "repo": repo })),
            )
            .await?;

        Ok(result
            .repository
            .issues
            .nodes
            .into_iter()
            .map(IssueWithParent::from)
            .collect())
    }

    // ── Branch methods (REST) ──────────────────────────────────

    /// Get the default branch name for a repository.
    pub async fn get_repo_default_branch(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<String, GitHubError> {
        let path = format!("/repos/{}/{}", owner, repo);
        let data = self.rest_get(&path, None).await?;

        let result: RestRepo = serde_json::from_value(data)?;
        Ok(result.default_branch)
    }

    /// Get the SHA of a specific commit ref in a repository.
    pub async fn get_repo_commit_sha(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
    ) -> Result<String, GitHubError> {
        let path = format!("/repos/{}/{}/commits/{}", owner, repo, git_ref);
        let data = self.rest_get(&path, None).await?;

        let result: RestCommit = serde_json::from_value(data)?;
        Ok(result.sha)
    }

    /// Create a new git branch ref via the REST API.
    pub async fn create_branch_ref(
        &self,
        owner: &str,
        repo: &str,
        branch_name: &str,
        sha: &str,
    ) -> Result<(), GitHubError> {
        let path = format!("/repos/{}/{}/git/refs", owner, repo);
        let body = json!({
            "ref": format!("refs/heads/{}", branch_name),
            "sha": sha,
        });

        self.rest_post(&path, &body).await?;
        Ok(())
    }

    /// Check whether a branch exists in a repository.
    ///
    /// Returns `Ok(true)` if the branch exists, `Ok(false)` if the API
    /// responds with 404, or `Err(HttpStatus(status))` for other errors.
    pub async fn branch_exists(
        &self,
        owner: &str,
        repo: &str,
        branch_name: &str,
    ) -> Result<bool, GitHubError> {
        let path = format!("/repos/{}/{}/branches/{}", owner, repo, branch_name);

        match self.rest_get(&path, None).await {
            Ok(_) => Ok(true),
            Err(GitHubError::HttpStatus(404)) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Helper: create a client pointed at a mock server.
    async fn make_client(mock: &MockServer) -> GitHubClient {
        GitHubClient::new_with_base_url("test-token".to_string(), mock.uri())
            .expect("token is non-empty")
    }

    // ── GraphQL transport ──────────────────────────────────

    /// Test 1: graphql<T> with mock returning {"data": {"createProjectV2": {"id": "pid1"}}} → deserializes correctly
    #[tokio::test]
    async fn graphql_deserializes_data() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "createProjectV2": { "id": "pid1" } }
            })))
            .mount(&mock)
            .await;

        let result: CreateProjectV2Result = client
            .graphql(r#"mutation { createProjectV2(input: {}) { id } }"#, None)
            .await
            .expect("graphql should succeed");

        assert_eq!(result.create_project_v2.id, "pid1");
    }

    /// Test 2: graphql<T> with mock returning 500 → Err(HttpStatus(500))
    #[tokio::test]
    async fn graphql_500_returns_http_status() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let result: Result<CreateProjectV2Result, _> = client
            .graphql(r#"mutation { createProjectV2(input: {}) { id } }"#, None)
            .await;

        assert!(matches!(result, Err(GitHubError::HttpStatus(500))));
    }

    /// Test 3: graphql<T> with mock returning {"errors": [{"message": "bad"}]} → Err(GraphQLError)
    #[tokio::test]
    async fn graphql_errors_returns_graphql_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{ "message": "bad" }]
            })))
            .mount(&mock)
            .await;

        let result: Result<CreateProjectV2Result, _> = client
            .graphql(r#"mutation { createProjectV2(input: {}) { id } }"#, None)
            .await;

        match result {
            Err(GitHubError::GraphQLError(msg)) => {
                assert!(msg.contains("bad") || msg.contains("errors"));
            }
            other => panic!("expected GraphQLError, got {:?}", other),
        }
    }

    /// Test 4: graphql<T> with mock returning {"data": null} → Err(GraphQLError)
    #[tokio::test]
    async fn graphql_null_data_returns_graphql_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": null
            })))
            .mount(&mock)
            .await;

        let result: Result<CreateProjectV2Result, _> = client
            .graphql(r#"mutation { createProjectV2(input: {}) { id } }"#, None)
            .await;

        assert!(matches!(result, Err(GitHubError::GraphQLError(_))));
    }

    // ── Constructor ──────────────────────────────────────────

    /// Test 5: GitHubClient::new("") → Err(EmptyToken)
    #[test]
    fn new_empty_token_returns_error() {
        let result = GitHubClient::new(String::new());
        assert!(matches!(result, Err(GitHubError::EmptyToken)));
    }

    /// Test 6: GitHubClient::new("token123") → Ok(...)
    #[test]
    fn new_valid_token_succeeds() {
        let result = GitHubClient::new("token123".to_string());
        assert!(result.is_ok());
    }

    // ── Project methods ──────────────────────────────────────

    /// Test 7: create_project → returns project ID
    #[tokio::test]
    async fn create_project_returns_id() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "user": { "id": "uid1" }, "organization": null }
            })))
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "createProjectV2": { "id": "pid1" } }
            })))
            .mount(&mock)
            .await;

        let result = client.create_project("octocat", "My Project").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "pid1");
    }

    /// Test 8: get_project → ProjectV2Summary with number as String
    #[tokio::test]
    async fn get_project_returns_summary() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "id": "p1", "number": 3, "title": "T" } }
            })))
            .mount(&mock)
            .await;

        let result = client
            .get_project("p1")
            .await
            .expect("get_project should succeed");
        assert_eq!(result.id, "p1");
        assert_eq!(result.number, "3");
        assert_eq!(result.title, "T");
    }

    /// Test 9: get_project with node=null → Err(ProjectNotFound)
    #[tokio::test]
    async fn get_project_null_node_returns_project_not_found() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": null }
            })))
            .mount(&mock)
            .await;

        let result = client.get_project("p1").await;
        assert!(matches!(result, Err(GitHubError::ProjectNotFound(_))));
    }

    /// Test 10: get_project_fields → Vec<ProjectFieldInfo> with data_type
    #[tokio::test]
    async fn get_project_fields_returns_fields() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                { "id": "f1", "name": "Status", "dataType": "SINGLE_SELECT" }
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        let result = client
            .get_project_fields("p1")
            .await
            .expect("should succeed");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "f1");
        assert_eq!(result[0].name, "Status");
        assert_eq!(result[0].data_type, "SINGLE_SELECT");
    }

    /// Test 11: get_project_status_field with field:null → Ok(None)
    #[tokio::test]
    async fn get_project_status_field_null_returns_none() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "field": null } }
            })))
            .mount(&mock)
            .await;

        let result = client
            .get_project_status_field("p1")
            .await
            .expect("should succeed");
        assert!(result.is_none());
    }

    /// Test 12: get_project_status_field with field present → Ok(Some(...))
    #[tokio::test]
    async fn get_project_status_field_present_returns_some() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "f1",
                            "options": [
                                { "id": "o1", "name": "Todo" }
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        let result = client
            .get_project_status_field("p1")
            .await
            .expect("should succeed");
        let field = result.expect("should have Some");
        assert_eq!(field.id, "f1");
        assert_eq!(field.options.len(), 1);
        assert_eq!(field.options[0].id, "o1");
        assert_eq!(field.options[0].name, "Todo");
    }

    /// Test 13: list_project_items with null content → filtered out
    #[tokio::test]
    async fn list_project_items_filters_null_content() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "items": {
                            "nodes": [
                                {
                                    "id": "item1",
                                    "content": null
                                },
                                {
                                    "id": "item2",
                                    "content": {
                                        "__typename": "Issue",
                                        "id": "c1",
                                        "number": 42
                                    }
                                }
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        let result = client
            .list_project_items("p1")
            .await
            .expect("should succeed");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "item2");
        assert_eq!(result[0].content_node_id, "c1");
        assert_eq!(result[0].content_type, "Issue");
        assert_eq!(result[0].content_number, 42);
    }

    /// Test 14: get_project_item_values → BTreeMap with text and option fallback
    #[tokio::test]
    async fn get_project_item_values_builds_map() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fieldValues": {
                            "nodes": [
                                { "name": "Status", "text": "Triage" },
                                { "name": "sessionId" }
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        let result = client
            .get_project_item_values("item1")
            .await
            .expect("should succeed");

        assert_eq!(result.get("Status"), Some(&Some("Triage".to_string())));
        assert_eq!(result.get("sessionId"), Some(&None));
    }

    // ── REST issue methods ───────────────────────────────────

    /// Test 15: list_repo_issues → filters out PRs (pull_request present)
    #[tokio::test]
    async fn list_repo_issues_filters_prs() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "node_id": "n1",
                    "number": 1,
                    "title": "Issue",
                    "body": "body text",
                    "state": "open",
                    "pull_request": null
                },
                {
                    "node_id": "n2",
                    "number": 2,
                    "title": "PR",
                    "body": null,
                    "state": "open",
                    "pull_request": { "url": "http://example.com" }
                }
            ])))
            .mount(&mock)
            .await;

        let result = client
            .list_repo_issues("owner", "repo")
            .await
            .expect("should succeed");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "n1");
        assert_eq!(result[0].title, "Issue");
    }

    /// Test 16: list_repo_issues with body:null → IssueInfo.body is None
    #[tokio::test]
    async fn list_repo_issues_null_body() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "node_id": "n1",
                    "number": 1,
                    "title": "No Body",
                    "body": null,
                    "state": "open",
                    "pull_request": null
                }
            ])))
            .mount(&mock)
            .await;

        let result = client
            .list_repo_issues("owner", "repo")
            .await
            .expect("should succeed");

        assert_eq!(result.len(), 1);
        assert!(result[0].body.is_none());
    }

    /// Test 17: search_issues → query param = repo:owner/repo {query}, items mapped
    #[tokio::test]
    async fn search_issues_returns_items() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/search/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [
                    {
                        "node_id": "s1",
                        "number": 10,
                        "title": "Search Result",
                        "body": "found it",
                        "state": "open"
                    }
                ]
            })))
            .mount(&mock)
            .await;

        let result = client
            .search_issues("owner", "repo", "is:open")
            .await
            .expect("should succeed");

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "s1");
        assert_eq!(result[0].number, 10);
        assert_eq!(result[0].title, "Search Result");
    }

    // ── REST branch methods ──────────────────────────────────

    /// Test 18: branch_exists returning 200 → true
    #[tokio::test]
    async fn branch_exists_200_returns_true() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/branches/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .mount(&mock)
            .await;

        let result = client
            .branch_exists("owner", "repo", "main")
            .await
            .expect("should succeed");
        assert!(result);
    }

    /// Test 19: branch_exists returning 404 → false
    #[tokio::test]
    async fn branch_exists_404_returns_false() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/branches/main"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({})))
            .mount(&mock)
            .await;

        let result = client
            .branch_exists("owner", "repo", "main")
            .await
            .expect("should succeed");
        assert!(!result);
    }

    /// Test 20: branch_exists returning 500 → Err(HttpStatus(500)) (NOT false)
    #[tokio::test]
    async fn branch_exists_500_returns_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/branches/main"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({})))
            .mount(&mock)
            .await;

        let result = client.branch_exists("owner", "repo", "main").await;
        assert!(matches!(result, Err(GitHubError::HttpStatus(500))));
    }

    /// Test 21: get_repo_default_branch → "main"
    #[tokio::test]
    async fn get_repo_default_branch_returns_branch() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "default_branch": "main"
            })))
            .mount(&mock)
            .await;

        let result = client
            .get_repo_default_branch("owner", "repo")
            .await
            .expect("should succeed");
        assert_eq!(result, "main");
    }

    /// Test 22: get_repo_commit_sha → "abc123"
    #[tokio::test]
    async fn get_repo_commit_sha_returns_sha() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/commits/main"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "sha": "abc123"
            })))
            .mount(&mock)
            .await;

        let result = client
            .get_repo_commit_sha("owner", "repo", "main")
            .await
            .expect("should succeed");
        assert_eq!(result, "abc123");
    }

    // ── Issue hierarchy (GraphQL) ──────────────────────────────

    /// Test 23: list_issues_with_parents → returns issues with parent_number
    #[tokio::test]
    async fn list_issues_with_parents_returns_hierarchy() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("parentIssue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "issues": {
                            "nodes": [
                                {
                                    "id": "i1",
                                    "number": 1,
                                    "title": "Parent issue",
                                    "body": "parent body",
                                    "state": "open",
                                    "parentIssue": null
                                },
                                {
                                    "id": "i2",
                                    "number": 2,
                                    "title": "Sub-issue 1",
                                    "body": null,
                                    "state": "open",
                                    "parentIssue": { "number": 1 }
                                },
                                {
                                    "id": "i3",
                                    "number": 3,
                                    "title": "Sub-issue 2",
                                    "body": "child body",
                                    "state": "closed",
                                    "parentIssue": { "number": 1 }
                                }
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        let result = client
            .list_issues_with_parents("owner", "repo")
            .await
            .expect("should succeed");

        assert_eq!(result.len(), 3);
        // Parent issue has no parent_number
        assert_eq!(result[0].number, 1);
        assert_eq!(result[0].title, "Parent issue");
        assert!(result[0].parent_number.is_none());
        // Sub-issues have parent_number = 1
        assert_eq!(result[1].number, 2);
        assert_eq!(result[1].parent_number, Some(1));
        assert_eq!(result[2].number, 3);
        assert_eq!(result[2].parent_number, Some(1));
    }

    /// Test 24: list_issues_with_parents with no sub-issues → all parent_number None
    #[tokio::test]
    async fn list_issues_with_parents_no_subtasks() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("parentIssue"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "issues": {
                            "nodes": [
                                {
                                    "id": "i1",
                                    "number": 1,
                                    "title": "Issue 1",
                                    "body": "body",
                                    "state": "open",
                                    "parentIssue": null
                                }
                            ]
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        let result = client
            .list_issues_with_parents("owner", "repo")
            .await
            .expect("should succeed");

        assert_eq!(result.len(), 1);
        assert!(result[0].parent_number.is_none());
    }

    /// Test 25: list_issues_with_parents handles 500 → HttpStatus
    #[tokio::test]
    async fn list_issues_with_parents_500_returns_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("parentIssue"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock)
            .await;

        let result = client.list_issues_with_parents("owner", "repo").await;
        assert!(matches!(result, Err(GitHubError::HttpStatus(500))));
    }
}
