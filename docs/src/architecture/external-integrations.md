# External Integrations

## GitHub API

The `GitHubClient` in `src/external_issues/github/client.rs` communicates with GitHub via two protocols:

### GraphQL

Used for Project V2 operations:

| Operation | Method | Description |
|-----------|--------|-------------|
| `graphql::<T>()` | `POST /graphql` | Generic GraphQL executor |
| `create_project()` | mutation `createProjectV2` | Create a new project board |
| `get_project_status_field()` | query `field(name: "Status")` | Get the Status single-select field |
| `get_project_fields()` | query `fields(first: 10)` | List all project fields |
| `list_project_items()` | query `items(first: 100)` | List project items |
| `add_issue_to_project()` | mutation `addProjectV2ItemById` | Add an issue to a project |
| `update_project_item_status()` | mutation `updateProjectV2ItemFieldValue` | Set status on an item |
| `get_project_item_values()` | query `fieldValues(first: 10)` | Get field values for an item |
| `add_project_status_options()` | mutation `updateProjectV2Field` | Replace single-select field options (existing + new) |
| `add_project_field()` | mutation `createProjectV2Field` | Add a custom field |

### REST

Used for repository-level operations:

| Operation | Method | Endpoint |
|-----------|--------|----------|
| `list_repo_issues()` | GET | `/repos/{owner}/{repo}/issues` |
| `get_repo_default_branch()` | GET | `/repos/{owner}/{repo}` |
| `get_repo_commit_sha()` | GET | `/repos/{owner}/{repo}/commits/{branch}` |
| `branch_exists()` | GET | `/repos/{owner}/{repo}/branches/{name}` |
| `create_branch_ref()` | POST | `/repos/{owner}/{repo}/git/refs` |

### Authentication

All requests use:
```
Authorization: Bearer {token}
User-Agent: git-automate
Accept: application/vnd.github+json
```

### Error type

```rust
pub enum GitHubError {
    EmptyToken,
    Http(#[from] reqwest::Error),
    HttpStatus(u16),
    GraphQLError(String),
    ProjectNotFound(String),
    OwnerNotFound(String),
    Other(String),
}
```

## OpenCode HTTP client

The `OpenCodeClient` in `src/external_agent/opencode/client.rs` communicates with the OpenCode server via HTTP REST.

### Authentication

HTTP Basic auth with username `opencode` and the configured password:
```
Authorization: Basic base64("opencode:<pw>")
```

### Endpoints

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/global/health` | GET | Health check |
| `/agent` | GET | List available agents |
| `/session` | POST | Create session + send prompt |
| `/session/{id}/prompt_async` | POST | Send async prompt |
| `/session/{id}/shell` | POST | Execute shell command |
| `/session/status` | GET | Get session status |
| `/session/{id}` | GET | Get session details |

### Error type

```rust
pub enum OpenCodeError {
    Http(#[from] reqwest::Error),
    HttpStatus(u16),
    FetchAgents(String),
    CreateSession(String),
    NoSessionData(String),
}
```

## Agent session abstraction

The `ExternalAgent` trait in `src/external_agent/common/mod.rs` defines a provider-agnostic interface:

```rust
pub trait ExternalAgent: Send + Sync {
    fn start_session(&self, project_key, system_prompt, user_prompt) -> Future<Output = Result<String, ExternalAgentError>>;
    fn execute_shell(&self, session_id, command) -> Future<Output = Result<String, ExternalAgentError>>;
    fn session_status(&self, session_id) -> Future<Output = Result<AgentSessionStatus, ExternalAgentError>>;
    fn nudge_session(&self, session_id) -> Future<Output = Result<(), ExternalAgentError>>;
    fn get_session(&self, session_id) -> Future<Output = Result<Option<SessionInfo>, ExternalAgentError>>;
    fn list_sessions(&self) -> Future<Output = Result<Vec<SessionInfo>, ExternalAgentError>>;
}
```

The OpenCode implementation (`src/external_agent/opencode/agent.rs`) maps these trait methods to the HTTP endpoints above.
