//! GitHub API client supporting both GraphQL (Projects V2) and REST operations.
//! Authentication uses a personal access token. GraphQL requests go to
//! `https://api.github.com/graphql`; REST requests go to `https://api.github.com/`.
//!
//! Uses `reqwest` as the HTTP transport. GraphQL operations POST directly
//! to the `/graphql` endpoint; REST operations use `reqwest` GET/POST for
//! direct status code and body inspection.

use super::types::*;
use reqwest::header::{self, HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use thiserror::Error;

// Re-export list-query types used by find_project_by_name.
use super::types::{OrgProjectsResult, UserProjectsResult};

// ─── Error type ───────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("GitHub token is required")]
    EmptyToken,
    #[error("HTTP error: {0}")]
    Http(String),
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

impl From<reqwest::Error> for GitHubError {
    fn from(e: reqwest::Error) -> Self {
        GitHubError::Other(e.to_string())
    }
}

// ─── Client ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GitHubClient {
    client: reqwest::Client,
    base_url: String,
}

impl GitHubClient {
    /// Create a new client targeting the real GitHub API.
    ///
    /// # Errors
    /// Returns `GitHubError::EmptyToken` if `token` is empty.
    pub fn new(token: String) -> Result<Self, GitHubError> {
        Self::new_with_base_url(token, "https://api.github.com".to_string())
    }

    /// Create a client targeting a custom base URL (for testing/integration).
    pub fn new_with_base_url(token: String, base_url: String) -> Result<Self, GitHubError> {
        if token.is_empty() {
            return Err(GitHubError::EmptyToken);
        }

        let mut headers = HeaderMap::new();
        headers.insert(header::USER_AGENT, HeaderValue::from_static("git-automate"));
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", token))
                .map_err(|e| GitHubError::Other(format!("invalid auth token: {}", e)))?,
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| GitHubError::Other(e.to_string()))?;

        Ok(Self { client, base_url })
    }

    // ── Core transport ──────────────────────────────────────────

    const MAX_ATTEMPTS: u8 = 4; // 1 initial + 3 retries on 5xx/network errors

    async fn execute_with_retry(
        &self,
        request: reqwest::Request,
    ) -> Result<reqwest::Response, GitHubError> {
        let mut last_error = String::new();

        for attempt in 1..=Self::MAX_ATTEMPTS {
            let req = request
                .try_clone()
                .ok_or_else(|| GitHubError::Other("request clone failed".to_string()))?;
            match self.client.execute(req).await {
                Ok(response) => {
                    if !response.status().is_server_error() {
                        return Ok(response);
                    }
                    if attempt < Self::MAX_ATTEMPTS {
                        continue;
                    }
                    return Ok(response);
                }
                Err(e) => {
                    last_error = e.to_string();
                    if attempt < Self::MAX_ATTEMPTS {
                        continue;
                    }
                }
            }
        }

        Err(GitHubError::Other(last_error))
    }

    /// Execute a GraphQL query or mutation via a direct POST to `/graphql`.
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

        let url = format!("{}/graphql", self.base_url);
        let request = self.client.post(&url).json(&payload).build()?;
        let response = self.execute_with_retry(request).await?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(GitHubError::HttpStatus(status));
        }

        let body: Value = response.json().await?;

        if let Some(errors) = body.get("errors") {
            return Err(GitHubError::GraphQLError(format!("{:?}", errors)));
        }

        let data = body.get("data").ok_or_else(|| {
            GitHubError::Other("GraphQL response missing 'data' field".to_string())
        })?;

        if data.is_null() {
            return Err(GitHubError::Other(
                "GraphQL response data is null".to_string(),
            ));
        }

        Ok(serde_json::from_value(data.clone())?)
    }

    /// GET `{base_url}{path}?{query}` (query is optional).
    /// Returns the parsed JSON body as a `Value`.
    /// On non-2xx: returns `Err(HttpStatus(status))`.
    async fn rest_get(&self, path: &str, query: Option<&str>) -> Result<Value, GitHubError> {
        let url = format!("{}{}", self.base_url, path);
        let mut request = self.client.get(&url);

        if let Some(q) = query {
            request = request.query(&[("q", q)]);
        }

        let request = request.build()?;
        let response = self.execute_with_retry(request).await?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(GitHubError::HttpStatus(status));
        }

        let body_str = response.text().await?;
        let body: Value = serde_json::from_str(&body_str)?;
        Ok(body)
    }

    /// POST `{base_url}{path}` with a JSON body.
    /// Returns the parsed JSON body as a `Value`.
    /// On non-2xx: returns `Err(HttpStatus(status))`.
    async fn rest_post(&self, path: &str, body: &Value) -> Result<Value, GitHubError> {
        let url = format!("{}{}", self.base_url, path);
        let request = self.client.post(&url).json(body).build()?;
        let response = self.execute_with_retry(request).await?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(GitHubError::HttpStatus(status));
        }

        let body_str = response.text().await?;
        let body: Value = serde_json::from_str(&body_str)?;
        Ok(body)
    }

    // ── Project V2 methods ─────────────────────────────────────

    /// Resolve a login to a node ID by trying both user and organization.
    async fn get_owner_id(&self, login: &str) -> Result<String, GitHubError> {
        let result = self
            .graphql::<NodeOwnerResult>(
                include_str!("queries/get_owner_id.graphql"),
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
                include_str!("queries/create_project.graphql"),
                Some(&json!({ "input": { "title": title, "ownerId": owner_id } })),
            )
            .await?;

        Ok(result.create_project_v2.project_v2.id)
    }

    /// Resolve a Project V2 by its number to a global node ID.
    ///
    /// The owner may be either a user or an organization. Sends two
    /// sequential queries — `user(login:)` first, then `organization(login:)`
    /// as a fallback. This avoids a GraphQL `NOT_FOUND` error on the
    /// `organization` field when the owner is a user (and vice versa),
    /// which would cause a single combined query to fail.
    pub async fn get_project_by_number(
        &self,
        owner: &str,
        number: i64,
    ) -> Result<String, GitHubError> {
        // Try user first.
        let result = self
            .graphql::<ProjectNumberResult>(
                include_str!("queries/get_project_by_number_user.graphql"),
                Some(&json!({ "owner": owner, "number": number })),
            )
            .await;

        if let Ok(result) = result
            && let Some(id) = result.user.and_then(|u| u.project_v2.map(|p| p.id))
        {
            return Ok(id);
        }

        // Fall back to organization.
        let result = self
            .graphql::<ProjectNumberResult>(
                include_str!("queries/get_project_by_number_org.graphql"),
                Some(&json!({ "owner": owner, "number": number })),
            )
            .await;

        if let Ok(result) = result
            && let Some(id) = result.organization.and_then(|o| o.project_v2.map(|p| p.id))
        {
            return Ok(id);
        }

        Err(GitHubError::ProjectNotFound(format!("project #{}", number)))
    }

    /// Fetch a Project V2 by node ID.
    pub async fn get_project(&self, project_id: &str) -> Result<ProjectV2Summary, GitHubError> {
        let result = self
            .graphql::<NodeProjectResult>(
                include_str!("queries/get_project.graphql"),
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
                include_str!("queries/get_project_fields.graphql"),
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
                include_str!("queries/add_project_field.graphql"),
                Some(&json!({
                    "input": {
                        "projectId": project_id,
                        "name": name,
                        "dataType": data_type,
                    }
                })),
            )
            .await?;

        Ok(result.create_project_v2_field.project_v2_field.id)
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
                include_str!("queries/get_project_status_field.graphql"),
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
        options: &[Value],
    ) -> Result<(), GitHubError> {
        self.graphql::<UpdateFieldConfigResult>(
            include_str!("queries/add_project_status_options.graphql"),
            Some(&json!({
                "input": {
                    "fieldId": field_id,
                    "singleSelectOptions": options,
                }
            })),
        )
        .await?;

        Ok(())
    }

    /// Remove stale options from a single-select field by updating it with
    /// only the remaining (non-stale) options.
    ///
    /// Fetches the current field options, filters out the given option IDs,
    /// and calls `updateProjectV2Field` with the filtered list.
    pub async fn remove_project_status_options(
        &self,
        field_id: &str,
        option_ids_to_remove: &[String],
    ) -> Result<(), GitHubError> {
        let result = self
            .graphql::<StatusFieldResult>(
                include_str!("queries/get_project_status_field.graphql"),
                Some(&json!({ "id": field_id, "name": "Status" })),
            )
            .await?;

        let field = match result.node.and_then(|n| n.field) {
            Some(f) => f,
            None => return Ok(()),
        };

        let options_len = field.options.len();
        let remaining: Vec<Value> = field
            .options
            .into_iter()
            .filter(|o| !option_ids_to_remove.contains(&o.id))
            .map(|o| json!({ "id": o.id, "name": o.name, "color": "GRAY", "description": "" }))
            .collect();

        if remaining.len() == options_len {
            return Ok(());
        }

        self.graphql::<UpdateFieldConfigResult>(
            include_str!("queries/remove_project_status_options.graphql"),
            Some(&json!({
                "input": {
                    "fieldId": field_id,
                    "singleSelectOptions": remaining,
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
                include_str!("queries/list_project_items.graphql"),
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
                let number = content.number?;
                Some(ProjectItem {
                    id: item.id,
                    content_node_id: content.id,
                    content_type: content.typename,
                    content_number: number,
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
            include_str!("queries/update_project_item_status.graphql"),
            Some(&json!({
                "input": {
                    "projectId": project_id,
                    "itemId": item_id,
                    "fieldId": field_id,
                    "value": { "singleSelectOptionId": option_id },
                }
            })),
        )
        .await?;

        Ok(())
    }

    /// Set a text field (e.g. session ID) value on a project item.
    ///
    /// Pass `Some(id)` to set the text value, or `None` to clear it (sends
    /// `null` to the GitHub GraphQL API, which properly removes the value
    /// so that `get_project_item_values` returns `None` for the field).
    pub async fn update_project_item_session_id(
        &self,
        project_id: &str,
        item_id: &str,
        field_id: &str,
        session_id: Option<&str>,
    ) -> Result<(), GitHubError> {
        self.graphql::<UpdateItemFieldValueResult>(
            include_str!("queries/update_project_item_session_id.graphql"),
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

    /// Set a number field (e.g. waveId) value on a project item.
    pub async fn update_project_item_wave_id(
        &self,
        project_id: &str,
        item_id: &str,
        field_id: &str,
        wave_id: i64,
    ) -> Result<(), GitHubError> {
        self.graphql::<UpdateItemFieldValueResult>(
            include_str!("queries/update_project_item_wave_id.graphql"),
            Some(&json!({
                "input": {
                    "projectId": project_id,
                    "itemId": item_id,
                    "fieldId": field_id,
                    "value": { "number": wave_id as f64 },
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
                include_str!("queries/add_issue_to_project.graphql"),
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
                include_str!("queries/get_project_item_values.graphql"),
                Some(&json!({ "id": item_id })),
            )
            .await?;

        let mut values: BTreeMap<String, Option<String>> = BTreeMap::new();

        if let Some(node) = result.node {
            for field in node.field_values.nodes {
                let (name, value) = match field {
                    NodeFieldValueNode::Text { text, field } => {
                        let Some(name) = field.and_then(|f| f.name) else {
                            continue;
                        };
                        (name, text)
                    }
                    NodeFieldValueNode::SingleSelect { name: value, field } => {
                        let Some(name) = field.and_then(|f| f.name) else {
                            continue;
                        };
                        (name, value)
                    }
                    NodeFieldValueNode::Number { number, field } => {
                        let Some(name) = field.and_then(|f| f.name) else {
                            continue;
                        };
                        (name, number.map(|n| n.to_string()))
                    }
                    NodeFieldValueNode::Date { date, field } => {
                        let Some(name) = field.and_then(|f| f.name) else {
                            continue;
                        };
                        (name, date)
                    }
                    NodeFieldValueNode::Iteration { title, field } => {
                        let Some(name) = field.and_then(|f| f.name) else {
                            continue;
                        };
                        (name, title)
                    }
                    NodeFieldValueNode::Other => continue,
                };
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

    /// Create a new issue in a repository via the REST API.
    pub async fn create_issue(
        &self,
        owner: &str,
        repo: &str,
        title: &str,
        body: &str,
    ) -> Result<RestCreatedIssue, GitHubError> {
        let path = format!("/repos/{}/{}/issues", owner, repo);
        let body_json = json!({ "title": title, "body": body });
        let data = self.rest_post(&path, &body_json).await?;

        let result: RestCreatedIssue = serde_json::from_value(data)?;
        Ok(result)
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
                include_str!("queries/list_issues_with_parents.graphql"),
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

    pub async fn find_project_by_name(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<String, GitHubError> {
        // Try user first.
        let result = self
            .graphql::<UserProjectsResult>(
                include_str!("queries/list_user_projects.graphql"),
                Some(&json!({ "login": owner })),
            )
            .await;

        if let Ok(result) = result
            && let Some(holder) = result.user
        {
            for project in holder.projects_v2.nodes {
                if project.title == name {
                    return Ok(project.id);
                }
            }
        }

        // Fall back to organization.
        let result = self
            .graphql::<OrgProjectsResult>(
                include_str!("queries/list_org_projects.graphql"),
                Some(&json!({ "login": owner })),
            )
            .await;

        if let Ok(result) = result
            && let Some(holder) = result.organization
        {
            for project in holder.projects_v2.nodes {
                if project.title == name {
                    return Ok(project.id);
                }
            }
        }

        Err(GitHubError::ProjectNotFound(format!(
            "project named '{}'",
            name
        )))
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

    // ── PR comment methods ─────────────────────────────────────

    /// List all review threads (file+line comments) on a pull request.
    pub async fn list_pr_review_comments(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
    ) -> Result<Vec<ReviewThreadNode>, GitHubError> {
        let result = self
            .graphql::<ReviewThreadsResult>(
                include_str!("queries/list_pr_review_threads.graphql"),
                Some(&json!({ "owner": owner, "repo": repo, "prNumber": pr_number })),
            )
            .await?;

        Ok(result.repository.pull_request.review_threads.nodes)
    }

    /// List all normal (issue-style) comments on a pull request.
    pub async fn list_pr_comments(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
    ) -> Result<Vec<IssueCommentNode>, GitHubError> {
        let result = self
            .graphql::<IssueCommentsResult>(
                include_str!("queries/list_pr_comments.graphql"),
                Some(&json!({ "owner": owner, "repo": repo, "prNumber": pr_number })),
            )
            .await?;

        Ok(result.repository.pull_request.comments.nodes)
    }

    /// Resolve a review thread (file+line comment thread) by thread node ID.
    pub async fn resolve_review_thread(&self, thread_id: &str) -> Result<(), GitHubError> {
        self.graphql::<ResolveReviewThreadResult>(
            include_str!("queries/resolve_review_thread.graphql"),
            Some(&json!({ "input": { "threadId": thread_id } })),
        )
        .await?;

        Ok(())
    }

    // ── PR lookup methods (REST) ───────────────────────────────

    /// Find a pull request for a given branch in a repository.
    ///
    /// Returns `Ok(Some(pr))` if a PR exists for the branch,
    /// `Ok(None)` if no PR is found, or `Err` for other HTTP errors.
    pub async fn get_pull_request_for_branch(
        &self,
        owner: &str,
        repo: &str,
        branch: &str,
    ) -> Result<Option<PrInfo>, GitHubError> {
        let path = format!("/repos/{}/{}/pulls", owner, repo);
        let url = format!(
            "{}{}?head={}:{}&state=all",
            self.base_url, path, owner, branch
        );
        let request = self.client.get(&url).build()?;
        let response = self.execute_with_retry(request).await?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(GitHubError::HttpStatus(status));
        }

        let body_str = response.text().await?;
        let prs: Vec<PrInfo> = serde_json::from_str(&body_str)?;

        Ok(prs.into_iter().next())
    }

    /// Get the diff for a pull request.
    pub async fn get_pr_diff(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
    ) -> Result<String, GitHubError> {
        let path = format!("/repos/{}/{}/pulls/{}", owner, repo, pr_number);
        let url = format!("{}{}", self.base_url, path);
        let request = self
            .client
            .get(&url)
            .header(
                header::ACCEPT,
                HeaderValue::from_static("application/vnd.github.v3.diff"),
            )
            .build()?;
        let response = self.execute_with_retry(request).await?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(GitHubError::HttpStatus(status));
        }

        Ok(response.text().await?)
    }

    /// Get the URL for a pull request.
    pub async fn get_pr_url(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
    ) -> Result<String, GitHubError> {
        let path = format!("/repos/{}/{}/pulls/{}", owner, repo, pr_number);
        let data = self.rest_get(&path, None).await?;
        let pr: PrInfo = serde_json::from_value(data)?;
        Ok(pr.url)
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
                "data": { "createProjectV2": { "projectV2": { "id": "pid1" } } }
            })))
            .mount(&mock)
            .await;

        let result: CreateProjectV2Result = client
            .graphql(include_str!("queries/create_project.graphql"), None)
            .await
            .expect("graphql should succeed");

        assert_eq!(result.create_project_v2.project_v2.id, "pid1");
    }

    /// Test 2: graphql<T> with mock returning 500 → Err(HttpStatus(500))
    #[tokio::test]
    async fn graphql_500_returns_http_status() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "message": "Internal Server Error"
            })))
            .mount(&mock)
            .await;

        let result: Result<CreateProjectV2Result, _> = client
            .graphql(include_str!("queries/create_project.graphql"), None)
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
            .graphql(include_str!("queries/create_project.graphql"), None)
            .await;

        match result {
            Err(GitHubError::GraphQLError(msg)) => {
                assert!(msg.contains("bad") || msg.contains("errors"));
            }
            other => panic!("expected GraphQLError, got {:?}", other),
        }
    }

    /// Test 4: graphql<T> with mock returning {"data": null} → Err
    #[tokio::test]
    async fn graphql_null_data_returns_error() {
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
            .graphql(include_str!("queries/create_project.graphql"), None)
            .await;

        assert!(result.is_err());
    }

    // ── Constructor ──────────────────────────────────────────

    /// Test 5: GitHubClient::new("") → Err(EmptyToken)
    #[test]
    fn new_empty_token_returns_error() {
        let result = GitHubClient::new(String::new());
        assert!(matches!(result, Err(GitHubError::EmptyToken)));
    }

    /// Test 6: GitHubClient::new("token123") → Ok(...)
    #[tokio::test]
    async fn new_valid_token_succeeds() {
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
                "data": { "createProjectV2": { "projectV2": { "id": "pid1" } } }
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

    /// Test: get_project_by_number with user project → returns global ID
    #[tokio::test]
    async fn get_project_by_number_user_project_returns_id() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": { "projectV2": { "id": "PVT-user-1" } },
                    "organization": null
                }
            })))
            .mount(&mock)
            .await;

        let result = client.get_project_by_number("octocat", 4).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "PVT-user-1");
    }

    /// Test: get_project_by_number with org project → returns global ID
    #[tokio::test]
    async fn get_project_by_number_org_project_returns_id() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": null,
                    "organization": { "projectV2": { "id": "PVT-org-1" } }
                }
            })))
            .mount(&mock)
            .await;

        let result = client.get_project_by_number("github", 4).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "PVT-org-1");
    }

    /// Test: get_project_by_number with project not found → Err(ProjectNotFound)
    #[tokio::test]
    async fn get_project_by_number_not_found_returns_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": { "projectV2": null },
                    "organization": { "projectV2": null }
                }
            })))
            .mount(&mock)
            .await;

        let result = client.get_project_by_number("octocat", 999).await;
        assert!(matches!(result, Err(GitHubError::ProjectNotFound(_))));
    }

    /// Test: get_project_by_number with user owner — user query succeeds,
    /// org query is never called (reproduces the combined-query NOT_FOUND bug).
    #[tokio::test]
    async fn get_project_by_number_user_owner_skips_org_query() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("user(login:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": { "projectV2": { "id": "PVT-user-1" } }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Org query should never fire — user query already found the project.
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("organization(login:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "errors": [{
                    "message": "Could not resolve to an Organization with the login of 'octocat'.",
                    "path": ["organization"],
                    "type": "NOT_FOUND"
                }]
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let result = client.get_project_by_number("octocat", 4).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "PVT-user-1");

        mock.verify().await;
    }

    /// Test: get_project_by_number with org owner — user query returns
    /// NOT_FOUND error, falls through to org query which succeeds.
    #[tokio::test]
    async fn get_project_by_number_org_owner_falls_through_user_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("user(login:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "user": null },
                "errors": [{
                    "message": "Could not resolve to a User with the login of 'github'.",
                    "path": ["user"],
                    "type": "NOT_FOUND"
                }]
            })))
            .expect(1)
            .mount(&mock)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("organization(login:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "organization": { "projectV2": { "id": "PVT-org-1" } }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let result = client.get_project_by_number("github", 4).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "PVT-org-1");

        mock.verify().await;
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
        assert_eq!(result[0].data_type, Some("SINGLE_SELECT".to_string()));
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
                                {
                                    "__typename": "ProjectV2ItemFieldSingleSelectValue",
                                    "name": "Triage",
                                    "field": {
                                        "__typename": "ProjectV2Field",
                                        "name": "Status"
                                    }
                                },
                                {
                                    "__typename": "ProjectV2ItemFieldTextValue",
                                    "text": null,
                                    "field": {
                                        "__typename": "ProjectV2Field",
                                        "name": "sessionId"
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
            .and(body_string_contains("parent {"))
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
                                    "parent": null
                                },
                                {
                                    "id": "i2",
                                    "number": 2,
                                    "title": "Sub-issue 1",
                                    "body": null,
                                    "state": "open",
                                    "parent": { "number": 1 }
                                },
                                {
                                    "id": "i3",
                                    "number": 3,
                                    "title": "Sub-issue 2",
                                    "body": "child body",
                                    "state": "closed",
                                    "parent": { "number": 1 }
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
            .and(body_string_contains("parent {"))
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
                                    "parent": null
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
            .and(body_string_contains("parent {"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "message": "Internal Server Error"
            })))
            .mount(&mock)
            .await;

        let result = client.list_issues_with_parents("owner", "repo").await;
        assert!(matches!(result, Err(GitHubError::HttpStatus(500))));
    }

    // ── add_project_status_options ──────────────────────────────

    /// Test 26: add_project_status_options succeeds; the `... on ProjectV2Field`
    /// inline fragment matcher is the regression guard (without it, reverting the
    /// GraphQL fix would go undetected because wiremock returns 404).
    #[tokio::test]
    async fn add_project_status_options_succeeds_with_inline_fragment() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2Field"))
            .and(body_string_contains("... on ProjectV2Field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2Field": {
                        "projectV2Field": { "id": "status-field-id" }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let options = vec![
            json!({"name": "Triage", "color": "GRAY", "description": ""}),
            json!({"name": "Todo", "color": "GRAY", "description": ""}),
        ];
        let result = client
            .add_project_status_options("status-field-id", &options)
            .await;

        assert!(
            result.is_ok(),
            "add_project_status_options should succeed with inline fragment"
        );
        mock.verify().await;
    }

    /// Test 27: add_project_status_options → 500 → HttpStatus(500)
    #[tokio::test]
    async fn add_project_status_options_500_returns_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2Field"))
            .and(body_string_contains("... on ProjectV2Field"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "message": "Internal Server Error"
            })))
            .mount(&mock)
            .await;

        let options = vec![json!({"name": "Todo", "color": "GRAY", "description": ""})];
        let result = client
            .add_project_status_options("status-field-id", &options)
            .await;

        assert!(matches!(result, Err(GitHubError::HttpStatus(500))));
    }

    // ── PR comment methods ──────────────────────────────────

    /// T28: list_pr_review_comments → returns threads with comments
    #[tokio::test]
    async fn list_pr_review_comments_returns_threads() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("reviewThreads"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "pullRequest": {
                            "reviewThreads": {
                                "nodes": [
                                    {
                                        "id": "thread1",
                                        "path": "src/main.rs",
                                        "line": 42,
                                        "originalLine": 38,
                                        "isResolved": false,
                                        "diffSide": "RIGHT",
                                        "comments": {
                                            "nodes": [
                                                {
                                                    "id": "comment1",
                                                    "body": "This needs a fix",
                                                    "createdAt": "2024-01-01T00:00:00Z",
                                                    "author": {"login": "reviewer"}
                                                }
                                            ]
                                        }
                                    },
                                    {
                                        "id": "thread2",
                                        "path": "src/lib.rs",
                                        "line": null,
                                        "originalLine": null,
                                        "isResolved": true,
                                        "diffSide": "LEFT",
                                        "comments": {
                                            "nodes": [
                                                {
                                                    "id": "comment2",
                                                    "body": "LGTM",
                                                    "createdAt": "2024-01-02T00:00:00Z",
                                                    "author": null
                                                }
                                            ]
                                        }
                                    }
                                ]
                            }
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        let result = client
            .list_pr_review_comments("owner", "repo", 1)
            .await
            .expect("should succeed");

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "thread1");
        assert_eq!(result[0].path, "src/main.rs");
        assert_eq!(result[0].line, Some(42));
        assert!(!result[0].is_resolved);
        assert_eq!(result[0].comments.nodes.len(), 1);
        assert_eq!(result[0].comments.nodes[0].body, "This needs a fix");
        assert_eq!(
            result[0].comments.nodes[0].author.as_ref().unwrap().login,
            "reviewer"
        );
        assert!(result[1].is_resolved);
        assert!(result[1].comments.nodes[0].author.is_none());
    }

    /// T29: list_pr_review_comments 500 → Err(HttpStatus(500))
    #[tokio::test]
    async fn list_pr_review_comments_500_returns_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("reviewThreads"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "message": "Internal Server Error"
            })))
            .mount(&mock)
            .await;

        let result = client.list_pr_review_comments("owner", "repo", 1).await;
        assert!(matches!(result, Err(GitHubError::HttpStatus(500))));
    }

    /// T30: list_pr_comments → returns normal comments
    #[tokio::test]
    async fn list_pr_comments_returns_comments() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("ListPrComments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "pullRequest": {
                            "comments": {
                                "nodes": [
                                    {
                                        "id": "comment1",
                                        "body": "Nice PR!",
                                        "createdAt": "2024-01-01T00:00:00Z",
                                        "author": {"login": "reviewer1"}
                                    },
                                    {
                                        "id": "comment2",
                                        "body": "LGTM",
                                        "createdAt": "2024-01-02T00:00:00Z",
                                        "author": {"login": "reviewer2"}
                                    }
                                ]
                            }
                        }
                    }
                }
            })))
            .mount(&mock)
            .await;

        let result = client
            .list_pr_comments("owner", "repo", 1)
            .await
            .expect("should succeed");

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "comment1");
        assert_eq!(result[0].body, "Nice PR!");
        assert_eq!(result[0].author.as_ref().unwrap().login, "reviewer1");
        assert_eq!(result[1].id, "comment2");
        assert_eq!(result[1].author.as_ref().unwrap().login, "reviewer2");
    }

    /// T31: list_pr_comments 500 → Err(HttpStatus(500))
    #[tokio::test]
    async fn list_pr_comments_500_returns_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("ListPrComments"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "message": "Internal Server Error"
            })))
            .mount(&mock)
            .await;

        let result = client.list_pr_comments("owner", "repo", 1).await;
        assert!(matches!(result, Err(GitHubError::HttpStatus(500))));
    }

    /// T32: resolve_review_thread → Ok(()) on success
    #[tokio::test]
    async fn resolve_review_thread_succeeds() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("resolveReviewThread"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "resolveReviewThread": {
                        "thread": {"id": "thread1"}
                    }
                }
            })))
            .mount(&mock)
            .await;

        let result = client.resolve_review_thread("thread1").await;
        assert!(result.is_ok());
    }

    /// T33: resolve_review_thread 500 → Err(HttpStatus(500))
    #[tokio::test]
    async fn resolve_review_thread_500_returns_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("resolveReviewThread"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "message": "Internal Server Error"
            })))
            .mount(&mock)
            .await;

        let result = client.resolve_review_thread("thread1").await;
        assert!(matches!(result, Err(GitHubError::HttpStatus(500))));
    }

    /// T34: remove_project_status_options fetches field, filters stale options,
    /// and calls updateProjectV2Field with remaining options only.
    #[tokio::test]
    async fn remove_project_status_options_removes_stale() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        // First call: get_project_status_field → returns 3 options (2 valid + 1 stale)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "sf",
                            "options": [
                                {"id": "o1", "name": "Triage"},
                                {"id": "o2", "name": "Todo"},
                                {"id": "o3", "name": "Obsolete"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Second call: remove_project_status_options → updateProjectV2Field with 2 remaining
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("RemoveProjectStatusOptions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "updateProjectV2Field": {
                        "projectV2Field": { "id": "sf" }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let result = client
            .remove_project_status_options("sf", &["o3".to_string()])
            .await;

        assert!(result.is_ok());
        mock.verify().await;
    }

    /// T35: remove_project_status_options with no stale options → no update call
    #[tokio::test]
    async fn remove_project_status_options_no_stale_returns_ok() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "sf",
                            "options": [
                                {"id": "o1", "name": "Triage"},
                                {"id": "o2", "name": "Todo"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Remove mutation should NOT be called — no stale options
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("RemoveProjectStatusOptions"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&mock)
            .await;

        let result = client
            .remove_project_status_options("sf", &["o99".to_string()])
            .await;

        assert!(result.is_ok());
        mock.verify().await;
    }

    /// T36: get_pull_request_for_branch → returns Some(PrInfo) when PR exists
    #[tokio::test]
    async fn get_pull_request_for_branch_returns_pr() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {
                    "number": 42,
                    "url": "https://api.github.com/repos/owner/repo/pulls/42",
                    "title": "Fix bug"
                }
            ])))
            .mount(&mock)
            .await;

        let result = client
            .get_pull_request_for_branch("owner", "repo", "issue-42")
            .await
            .expect("should succeed");

        assert!(result.is_some());
        let pr = result.unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.url, "https://api.github.com/repos/owner/repo/pulls/42");
        assert_eq!(pr.title, "Fix bug");
    }

    /// T37: get_pull_request_for_branch → returns None when no PR exists
    #[tokio::test]
    async fn get_pull_request_for_branch_returns_none() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&mock)
            .await;

        let result = client
            .get_pull_request_for_branch("owner", "repo", "issue-99")
            .await
            .expect("should succeed");

        assert!(result.is_none());
    }

    /// T38: get_pull_request_for_branch → 500 returns error
    #[tokio::test]
    async fn get_pull_request_for_branch_500_returns_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/pulls"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({})))
            .mount(&mock)
            .await;

        let result = client
            .get_pull_request_for_branch("owner", "repo", "issue-42")
            .await;

        assert!(matches!(result, Err(GitHubError::HttpStatus(500))));
    }

    /// T39: get_pr_diff → returns diff string
    #[tokio::test]
    async fn get_pr_diff_returns_diff() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/pulls/42"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("diff --git a/main.rs b/main.rs\n+println!(\"hello\");\n"),
            )
            .mount(&mock)
            .await;

        let result = client
            .get_pr_diff("owner", "repo", 42)
            .await
            .expect("should succeed");

        assert!(result.contains("diff --git"));
        assert!(result.contains("println!(\"hello\");"));
    }

    /// T40: get_pr_diff → 404 returns error
    #[tokio::test]
    async fn get_pr_diff_404_returns_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/pulls/99"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({})))
            .mount(&mock)
            .await;

        let result = client.get_pr_diff("owner", "repo", 99).await;
        assert!(matches!(result, Err(GitHubError::HttpStatus(404))));
    }

    /// T41: get_pr_url → returns PR URL
    #[tokio::test]
    async fn get_pr_url_returns_url() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/pulls/42"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": 42,
                "url": "https://api.github.com/repos/owner/repo/pulls/42",
                "title": "Fix bug"
            })))
            .mount(&mock)
            .await;

        let result = client
            .get_pr_url("owner", "repo", 42)
            .await
            .expect("should succeed");

        assert_eq!(result, "https://api.github.com/repos/owner/repo/pulls/42");
    }

    /// T42: get_pr_url → 404 returns error
    #[tokio::test]
    async fn get_pr_url_404_returns_error() {
        let mock = MockServer::start().await;
        let client = make_client(&mock).await;

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/pulls/99"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({})))
            .mount(&mock)
            .await;

        let result = client.get_pr_url("owner", "repo", 99).await;
        assert!(matches!(result, Err(GitHubError::HttpStatus(404))));
    }
}
