//! git-automate: A standalone Rust daemon that automates GitHub issue workflows
//! via OpenCode agent sessions.

/// Configuration parsing (YAML + `${env:VAR}` substitution).
pub mod config;
/// Shell command execution abstraction.
pub mod shell;

/// OpenCode HTTP client for agent session management.
pub mod external_agent;
/// GitHub API client for issue + project operations.
pub mod external_issues;
/// Workflow engine — triage, todo, review, and QA orchestration.
pub mod workflow;

// Not gated by #[cfg(test)] so integration tests (which compile the library
// as an external crate) can also use SET_CWD_MUTEX to serialize cwd changes.
pub mod test_utils {
    use std::sync::Arc;
    use std::sync::LazyLock;
    use tokio::sync::Mutex;

    use crate::shell::{ShellFn, ShellOutput};

    pub static SET_CWD_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    /// A shell function that always succeeds with empty output.
    pub fn mock_shell() -> ShellFn {
        Arc::new(|_cmd: String| {
            Box::pin(async move {
                ShellOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                }
            })
        })
    }

    /// Build a `GitHubClient` pointed at a mock server.
    #[cfg(test)]
    pub fn gh_client(
        server: &wiremock::MockServer,
    ) -> crate::external_issues::github::client::GitHubClient {
        crate::external_issues::github::client::GitHubClient::new_with_base_url(
            "test-token".to_string(),
            server.uri(),
        )
        .expect("token is non-empty")
    }

    /// Build a `WorkflowContext` with an empty config and the given GitHub client.
    #[cfg(test)]
    pub fn make_deps(
        github: Option<crate::external_issues::github::client::GitHubClient>,
    ) -> crate::workflow::helpers::WorkflowContext {
        crate::workflow::helpers::WorkflowContext {
            config: crate::config::GitAutomateConfig {
                git: crate::config::GitSection::default(),
                concurrency: None,
                github_token: None,
                opencode: None,
            },
            github,
            shell: mock_shell(),
            project_id_cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }
}
