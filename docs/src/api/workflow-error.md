# WorkflowError

Errors returned by the workflow engine. Defined in `src/workflow/helpers.rs`.

```rust
pub enum WorkflowError {
    NoGitHub(String),
    NoStatusField(String),
    NoSessionField(String),
    StatusOptionNotFound(String),
    CloneFailed(String),
    ConfigNotFound(String),
    TemplateNotFound(String),
    Yaml(#[from] serde_yaml::Error),
    Io(#[from] std::io::Error),
    GitHub(#[from] GitHubError),
    Other(String),
}
```

| Variant | Cause |
|---------|-------|
| `NoGitHub(project)` | GitHub client not available for the given project |
| `NoStatusField(project)` | Project has no `Status` single-select field |
| `NoSessionField(project)` | Project has no `sessionId` text field |
| `StatusOptionNotFound(name)` | Named status option not found in the Status field |
| `CloneFailed(url)` | `git clone` exited with non-zero status |
| `ConfigNotFound(path)` | Config file not found when writing `projectId` |
| `TemplateNotFound(name)` | Prompt template not found for the given name |
| `Yaml` | Serde YAML error (wraps `serde_yaml::Error`) |
| `Io` | Standard IO error |
| `GitHub` | GitHub API error (wraps `GitHubError`) |
| `Other(msg)` | Generic error with a message |

## GitHubError

Errors from the GitHub API client. Defined in `src/external_issues/github/client.rs`.

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

## OpenCodeError

Errors from the OpenCode HTTP client. Defined in `src/external_agent/opencode/client.rs`.

```rust
pub enum OpenCodeError {
    Http(#[from] reqwest::Error),
    HttpStatus(u16),
    FetchAgents(String),
    CreateSession(String),
    NoSessionData(String),
}
```
