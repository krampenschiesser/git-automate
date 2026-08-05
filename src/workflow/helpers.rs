//! Shared workflow helpers — port of `src/workflow-helpers.ts`.
//!
//! Defines: shared types (`ProjectContext`, `FieldIds`, dependency structs),
//! prompt template loading/filling, repo cloning, config writing, and
//! context + field resolution used by every workflow check.

use std::collections::HashMap;

use regex::Regex;

use crate::config::{GitAutomateConfig, ProjectConfig};
use crate::external_issues::client::{GitHubClient, GitHubError};
use crate::external_issues::repo::parse_repository_url;
use crate::external_issues::types::{IssueInfo, ParsedRepo, StatusOption};
use crate::shell::{ShellFn, ShellOutput};

// ─── Constants ────────────────────────────────────────────────

/// Temporary working directory for cloned repos (`/tmp/git-automate-work`).
pub const WORK_DIR: &str = "/tmp/git-automate-work";

/// Name of the project's "Status" single-select field.
pub const STATUS_FIELD_NAME: &str = "Status";

/// Name of the custom text field that stores the OpenCode session ID.
pub const SESSION_FIELD_NAME: &str = "sessionId";

// ─── Error type ───────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("GitHub client not available for project {0}")]
    NoGitHub(String),
    #[error("Project {0} has no Status field")]
    NoStatusField(String),
    #[error("Project {0} has no sessionId field")]
    NoSessionField(String),
    #[error("Status option '{0}' not found")]
    StatusOptionNotFound(String),
    #[error("Failed to clone: {0}")]
    CloneFailed(String),
    #[error("Config file not found: {0}")]
    ConfigNotFound(String),
    #[error("Prompt template '{0}' not found")]
    TemplateNotFound(String),
    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("GitHub error: {0}")]
    GitHub(#[from] GitHubError),
    #[error("{0}")]
    Other(String),
}

// ─── Core types ───────────────────────────────────────────────

/// Fully-resolved project context used by workflow checks.
#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub name: String,
    pub config: ProjectConfig,
    pub owner: String,
    pub repo: String,
    pub project_id: String,
}

/// Resolved IDs for the Status field and (optionally) the sessionId field.
#[derive(Debug, Clone)]
pub struct FieldIds {
    pub status_field_id: String,
    pub session_field_id: Option<String>,
}

/// Dependencies for shell-based helpers (e.g. `clone_repo_if_needed`).
#[derive(Clone)]
pub struct ShellDeps {
    pub shell: ShellFn,
}

/// Dependencies for context resolution (`resolve_context`).
#[derive(Clone)]
pub struct ContextDeps {
    pub github: Option<GitHubClient>,
    pub config: GitAutomateConfig,
}

/// Full dependency set for workflow checks and the orchestrator
/// (equivalent to `WorkflowContext` from `src/workflow.ts`).
#[derive(Clone)]
pub struct WorkflowContext {
    pub config: GitAutomateConfig,
    pub github: Option<GitHubClient>,
    pub shell: ShellFn,
}

impl WorkflowContext {
    /// Convenience: extract the shell-related dependencies.
    pub fn shell_deps(&self) -> ShellDeps {
        ShellDeps {
            shell: self.shell.clone(),
        }
    }

    /// Convenience: extract the context-resolution dependencies.
    pub fn context_deps(&self) -> ContextDeps {
        ContextDeps {
            github: self.github.clone(),
            config: self.config.clone(),
        }
    }
}

// ─── Prompt helpers ───────────────────────────────────────────

/// Find the option ID for a given status name within a status field.
///
/// Equivalent to TS `resolveOptionId`.
pub fn resolve_option_id(options: &[StatusOption], name: &str) -> Option<String> {
    options
        .iter()
        .find(|opt| opt.name == name)
        .map(|opt| opt.id.clone())
}

/// Read a prompt template from compile-time embedded assets.
///
/// Uses `include_str!` — no runtime file access. Equivalent to TS
/// `loadPromptTemplate`, which reads from `prompts/<name>.md`.
pub fn load_prompt_template(name: &str) -> Result<String, WorkflowError> {
    let content = match name {
        "triage" => include_str!("../assets/prompts/triage.md"),
        "taskmanager" => include_str!("../assets/prompts/taskmanager.md"),
        "developer" => include_str!("../assets/prompts/developer.md"),
        "reviewer" => include_str!("../assets/prompts/reviewer.md"),
        "product" => include_str!("../assets/prompts/product.md"),
        "qa" => include_str!("../assets/prompts/qa.md"),
        _ => return Err(WorkflowError::TemplateNotFound(name.to_string())),
    };
    Ok(content.to_string())
}

/// Replace every `{{KEY}}` occurrence in *template* with the corresponding
/// value from *values*. Unknown keys are replaced with an empty string.
///
/// Regex: `\{\{\s*([A-Z_]+)\s*\}\}` — matches `{{KEY}}`, `{{ KEY }}`, etc.
/// Uses a `static OnceLock<Regex>` so the regex is compiled only once.
pub fn fill_prompt(template: &str, values: &HashMap<String, String>) -> String {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"\{\{\s*([A-Z_]+)\s*\}\}").expect("hardcoded regex literal is valid")
    });
    let filled = re.replace_all(template, |caps: &regex::Captures| {
        let key = caps.get(1).map_or("", |m| m.as_str());
        values.get(key).cloned().unwrap_or_default()
    });
    filled.into_owned()
}

// ─── Repo & config helpers ────────────────────────────────────

/// Clone a repository to [`WORK_DIR`] if not already present.
/// Returns the local path (`{WORK_DIR}/{owner}-{repo}`).
///
/// Equivalent to TS `cloneRepoIfNeeded`: uses `GITHUB_TOKEN` env var for
/// authenticated clone URL when set.
pub async fn clone_repo_if_needed(
    deps: &ShellDeps,
    owner: &str,
    repo: &str,
) -> Result<String, WorkflowError> {
    let clone_path = format!("{}/{}-{}", WORK_DIR, owner, repo);

    if std::path::Path::new(&clone_path).exists() {
        tracing::info!("Repo already cloned at {}", clone_path);
        return Ok(clone_path);
    }

    let parent = std::path::Path::new(&clone_path)
        .parent()
        .expect("clone path always has a parent");
    std::fs::create_dir_all(parent)?;

    let token = std::env::var("GITHUB_TOKEN").ok();
    let repo_url = if let Some(t) = token {
        format!(
            "https://x-access-token:{}@github.com/{}/{}.git",
            t, owner, repo
        )
    } else {
        format!("https://github.com/{}/{}.git", owner, repo)
    };

    let shell = deps.shell.clone();
    let output: ShellOutput =
        shell(format!("git clone --depth 1 {} {}", repo_url, clone_path)).await;
    if output.exit_code != 0 {
        return Err(WorkflowError::CloneFailed(format!(
            "{}: {}",
            repo_url, output.stderr
        )));
    }

    tracing::info!("Cloned {}/{} to {}", owner, repo, clone_path);
    Ok(clone_path)
}

/// Read `git-automate.yml`, set the `projectId` for a named project, and
/// write it back to disk — also updating the in-memory config.
///
/// Equivalent to TS `writeProjectId`. Uses `serde_yaml::Value` for
/// round-trip preservation of the YAML structure.
pub async fn write_project_id(
    project_name: &str,
    project_id: &str,
    config: &mut GitAutomateConfig,
) -> Result<(), WorkflowError> {
    let config_path = std::env::current_dir()?.join("git-automate.yml");
    if !config_path.exists() {
        return Err(WorkflowError::ConfigNotFound(
            config_path.display().to_string(),
        ));
    }

    let content = std::fs::read_to_string(&config_path)?;
    let mut data: serde_yaml::Value = serde_yaml::from_str(&content)?;

    // Ensure the root is a mapping.
    let root = data
        .as_mapping_mut()
        .ok_or_else(|| WorkflowError::Other("config root is not a mapping".to_string()))?;

    // Ensure "projects" exists.
    let proj_key = serde_yaml::Value::String("projects".to_string());
    if !root.contains_key(&proj_key) {
        root.insert(
            proj_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }

    let projects = root
        .get_mut(&proj_key)
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| WorkflowError::Other("projects is not a mapping".to_string()))?;

    // Ensure the named project entry exists.
    let name_key = serde_yaml::Value::String(project_name.to_string());
    if !projects.contains_key(&name_key) {
        projects.insert(
            name_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }

    let project = projects
        .get_mut(&name_key)
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| WorkflowError::Other("project entry is not a mapping".to_string()))?;

    project.insert(
        serde_yaml::Value::String("projectId".to_string()),
        serde_yaml::Value::String(project_id.to_string()),
    );

    // Write back to disk.
    let yaml_content = serde_yaml::to_string(&data)?;
    std::fs::write(&config_path, yaml_content)?;

    // Keep in-memory config in sync.
    if let Some(p) = config.projects.get_mut(project_name) {
        p.project_id = Some(project_id.to_string());
    }

    Ok(())
}

/// Build a `HashMap` of issue number → `IssueInfo` for a repository.
///
/// Equivalent to TS `getIssueBodyMap`.
pub async fn get_issue_body_map(
    github: &GitHubClient,
    owner: &str,
    repo: &str,
) -> Result<HashMap<i64, IssueInfo>, WorkflowError> {
    let issues = github.list_repo_issues(owner, repo).await?;
    let mut map = HashMap::new();
    for issue in issues {
        map.insert(issue.number, issue);
    }
    Ok(map)
}

/// Return the issue body, falling back to `Issue #{number}: {title}` if body is null.
///
/// Equivalent to TS `issueBodyOrTitle` (`issue.body ?? \`Issue #${issue.number}: ${issue.title}\``).
pub fn issue_body_or_title(issue: &IssueInfo) -> String {
    issue
        .body
        .clone()
        .unwrap_or_else(|| format!("Issue #{}: {}", issue.number, issue.title))
}

// ─── Context resolution ───────────────────────────────────────

/// Resolve a project name + config into a fully populated [`ProjectContext`],
/// creating the project on GitHub and persisting the ID if it does not yet exist.
///
/// Equivalent to TS `resolveContext`.
pub async fn resolve_context(
    deps: &ContextDeps,
    project_name: &str,
    project_config: &ProjectConfig,
) -> Result<ProjectContext, WorkflowError> {
    let github = deps
        .github
        .as_ref()
        .ok_or_else(|| WorkflowError::NoGitHub(project_name.to_string()))?;

    let ParsedRepo { owner, repo } = parse_repository_url(&project_config.repository)
        .map_err(|e| WorkflowError::Other(e.to_string()))?;

    let mut config = deps.config.clone();

    let project_id = if let Some(pid) = &project_config.project_id {
        pid.clone()
    } else {
        tracing::info!("Creating project {} for {}/{}", project_name, owner, repo);
        let pid = github.create_project(&owner, project_name).await?;
        write_project_id(project_name, &pid, &mut config).await?;
        pid
    };

    Ok(ProjectContext {
        name: project_name.to_string(),
        config: project_config.clone(),
        owner,
        repo,
        project_id,
    })
}

/// Resolve the Status and sessionId field IDs for a project.
///
/// Equivalent to TS `resolveFieldIds`.
pub async fn resolve_field_ids(
    github: &GitHubClient,
    project_id: &str,
) -> Result<FieldIds, WorkflowError> {
    let status_field = github
        .get_project_status_field(project_id)
        .await?
        .ok_or_else(|| WorkflowError::NoStatusField(project_id.to_string()))?;

    let fields = github.get_project_fields(project_id).await?;
    let session_field = fields
        .iter()
        .find(|f| f.name == SESSION_FIELD_NAME)
        .map(|f| f.id.clone());

    Ok(FieldIds {
        status_field_id: status_field.id,
        session_field_id: session_field,
    })
}

/// Combined resolver: fetches the Status field, resolves a named option
/// within it, and resolves the sessionId field — all in one call.
///
/// Equivalent to TS `resolveStatusOptionAndSession`.
/// Returns `(statusFieldId, optionId, sessionFieldId)`.
pub async fn resolve_status_option_and_session(
    github: &GitHubClient,
    project_id: &str,
    status_name: &str,
) -> Result<(String, String, String), WorkflowError> {
    let status_field = github
        .get_project_status_field(project_id)
        .await?
        .ok_or_else(|| WorkflowError::NoStatusField(project_id.to_string()))?;

    let option_id = resolve_option_id(&status_field.options, status_name)
        .ok_or_else(|| WorkflowError::StatusOptionNotFound(status_name.to_string()))?;

    let fields = github.get_project_fields(project_id).await?;
    let session_field = fields
        .iter()
        .find(|f| f.name == SESSION_FIELD_NAME)
        .map(|f| f.id.clone())
        .ok_or_else(|| WorkflowError::NoSessionField(project_id.to_string()))?;

    Ok((status_field.id, option_id, session_field))
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    // ── resolve_option_id tests ────────────────────────────────

    // Test 1: resolve_option_id with matching name → Some(id)
    #[test]
    fn resolve_option_id_matching_name() {
        let options = vec![
            StatusOption {
                id: "o1".to_string(),
                name: "Todo".to_string(),
            },
            StatusOption {
                id: "o2".to_string(),
                name: "In Progress".to_string(),
            },
        ];
        let result = resolve_option_id(&options, "In Progress");
        assert_eq!(result, Some("o2".to_string()));
    }

    // Test 2: resolve_option_id with no match → None
    #[test]
    fn resolve_option_id_no_match() {
        let options = vec![StatusOption {
            id: "o1".to_string(),
            name: "Todo".to_string(),
        }];
        let result = resolve_option_id(&options, "Done");
        assert_eq!(result, None);
    }

    // ── load_prompt_template tests ─────────────────────────────

    // Test 3: load_prompt_template("triage") → returns content containing {{ISSUE_TITLE}}
    #[test]
    fn load_prompt_template_triage() {
        let content = load_prompt_template("triage").expect("triage template should load");
        assert!(
            content.contains("{{ISSUE_TITLE}}"),
            "triage template should contain {{ISSUE_TITLE}}"
        );
    }

    // Test 4: load_prompt_template("nonexistent") → Err(TemplateNotFound)
    #[test]
    fn load_prompt_template_nonexistent() {
        let result = load_prompt_template("nonexistent");
        assert!(matches!(
            result,
            Err(WorkflowError::TemplateNotFound(name)) if name == "nonexistent"
        ));
    }

    // ── fill_prompt tests ──────────────────────────────────────

    // Test 5: fill_prompt("{{ISSUE_TITLE}}", {"ISSUE_TITLE": "Fix bug"}) → "Fix bug"
    #[test]
    fn fill_prompt_single_placeholder() {
        let template = "{{ISSUE_TITLE}}".to_string();
        let mut values = HashMap::new();
        values.insert("ISSUE_TITLE".to_string(), "Fix bug".to_string());
        let result = fill_prompt(&template, &values);
        assert_eq!(result, "Fix bug");
    }

    // Test 6: fill_prompt("{{ ISSUE_NUMBER }}", {"ISSUE_NUMBER": "42"}) → "42"
    #[test]
    fn fill_prompt_whitespace_in_braces() {
        let template = "{{ ISSUE_NUMBER }}".to_string();
        let mut values = HashMap::new();
        values.insert("ISSUE_NUMBER".to_string(), "42".to_string());
        let result = fill_prompt(&template, &values);
        assert_eq!(result, "42");
    }

    // Test 7: fill_prompt("{{UNKNOWN}}", {}) → ""
    #[test]
    fn fill_prompt_unknown_key_empty() {
        let template = "{{UNKNOWN}}".to_string();
        let values: HashMap<String, String> = HashMap::new();
        let result = fill_prompt(&template, &values);
        assert_eq!(result, "");
    }

    // Test 8: fill_prompt("no placeholders", {"X": "y"}) → "no placeholders"
    #[test]
    fn fill_prompt_no_placeholders() {
        let template = "no placeholders".to_string();
        let mut values = HashMap::new();
        values.insert("X".to_string(), "y".to_string());
        let result = fill_prompt(&template, &values);
        assert_eq!(result, "no placeholders");
    }

    // Test 9: fill_prompt with multiple placeholders → all replaced
    #[test]
    fn fill_prompt_multiple_placeholders() {
        let template = "{{ISSUE_TITLE}} by {{ISSUE_AUTHOR}}".to_string();
        let mut values = HashMap::new();
        values.insert("ISSUE_TITLE".to_string(), "Fix bug".to_string());
        values.insert("ISSUE_AUTHOR".to_string(), "alice".to_string());
        let result = fill_prompt(&template, &values);
        assert_eq!(result, "Fix bug by alice");
    }

    // ── issue_body_or_title tests ──────────────────────────────

    // Test 10: issue_body_or_title with body present → returns body
    #[test]
    fn issue_body_or_title_with_body() {
        let issue = IssueInfo {
            id: "n1".to_string(),
            number: 42,
            title: "My Issue".to_string(),
            body: Some("Detailed description".to_string()),
            state: "open".to_string(),
        };
        let result = issue_body_or_title(&issue);
        assert_eq!(result, "Detailed description");
    }

    // Test 11: issue_body_or_title with body None → returns "Issue #42: {title}"
    #[test]
    fn issue_body_or_title_without_body() {
        let issue = IssueInfo {
            id: "n1".to_string(),
            number: 42,
            title: "My Issue".to_string(),
            body: None,
            state: "open".to_string(),
        };
        let result = issue_body_or_title(&issue);
        assert_eq!(result, "Issue #42: My Issue");
    }

    // ── Constants tests ────────────────────────────────────────

    // Test 12: Constants match TS values
    #[test]
    fn constants_match_ts_values() {
        assert_eq!(WORK_DIR, "/tmp/git-automate-work");
        assert_eq!(STATUS_FIELD_NAME, "Status");
        assert_eq!(SESSION_FIELD_NAME, "sessionId");
    }

    // ── write_project_id tests ─────────────────────────────────

    #[tokio::test]
    async fn write_project_id_creates_and_updates() {
        let yaml = "projects:\n  my-repo:\n    repository: https://github.com/user/repo\n";
        let tmp_dir = tempfile::tempdir().unwrap();
        let config_path = tmp_dir.path().join("git-automate.yml");
        std::fs::write(&config_path, yaml).unwrap();

        // Load config so the HashMap has the project entry.
        let mut config = crate::config::parse_config(&config_path)
            .map_err(|e| format!("parse failed: {}", e))
            .unwrap();
        assert_eq!(config.projects.get("my-repo").unwrap().project_id, None);

        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().unwrap();
        let original_dir = env::current_dir().unwrap();
        env::set_current_dir(tmp_dir.path()).unwrap();

        write_project_id("my-repo", "PID-123", &mut config)
            .await
            .unwrap();

        env::set_current_dir(&original_dir).unwrap();

        // Verify in-memory config updated.
        assert_eq!(
            config
                .projects
                .get("my-repo")
                .unwrap()
                .project_id
                .as_deref(),
            Some("PID-123")
        );

        // Verify on-disk YAML updated.
        let written = std::fs::read_to_string(&config_path).unwrap();
        let data: serde_yaml::Value = serde_yaml::from_str(&written).unwrap();
        let pid = data
            .get("projects")
            .and_then(|p| p.get("my-repo"))
            .and_then(|p| p.get("projectId"))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(pid, "PID-123");
    }

    #[tokio::test]
    async fn write_project_id_missing_file() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let mut config = GitAutomateConfig {
            projects: std::collections::BTreeMap::new(),
        };

        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().unwrap();
        let original_dir = env::current_dir().unwrap();
        env::set_current_dir(tmp_dir.path()).unwrap();

        let result = write_project_id("ghost", "PID", &mut config).await;

        env::set_current_dir(&original_dir).unwrap();

        assert!(matches!(result, Err(WorkflowError::ConfigNotFound(_))));
    }

    #[tokio::test]
    async fn write_project_id_creates_new_project_entry() {
        let yaml = "projects:\n  existing:\n    repository: https://github.com/u/r\n";
        let tmp_dir = tempfile::tempdir().unwrap();
        let config_path = tmp_dir.path().join("git-automate.yml");
        std::fs::write(&config_path, yaml).unwrap();

        let mut config = crate::config::parse_config(&config_path)
            .map_err(|e| format!("parse failed: {}", e))
            .unwrap();
        assert!(config.projects.get("new-project").is_none());

        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().unwrap();
        let original_dir = env::current_dir().unwrap();
        env::set_current_dir(tmp_dir.path()).unwrap();

        write_project_id("new-project", "PID-NEW", &mut config)
            .await
            .unwrap();

        env::set_current_dir(&original_dir).unwrap();

        let written = std::fs::read_to_string(&config_path).unwrap();
        let data: serde_yaml::Value = serde_yaml::from_str(&written).unwrap();
        let pid = data
            .get("projects")
            .and_then(|p| p.get("new-project"))
            .and_then(|p| p.get("projectId"))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(pid, "PID-NEW");
    }

    // ── fill_prompt: no placeholders in template ───────────────

    #[test]
    fn fill_prompt_preserves_surrounding_text() {
        let template = "Hello {{NAME}}, welcome to {{PLACE}}!".to_string();
        let mut values = HashMap::new();
        values.insert("NAME".to_string(), "Alice".to_string());
        values.insert("PLACE".to_string(), "Wonderland".to_string());
        let result = fill_prompt(&template, &values);
        assert_eq!(result, "Hello Alice, welcome to Wonderland!");
    }

    // ── resolve_option_id: empty options → None ────────────────

    #[test]
    fn resolve_option_id_empty_list() {
        let options: Vec<StatusOption> = vec![];
        let result = resolve_option_id(&options, "Todo");
        assert_eq!(result, None);
    }

    // ── load_prompt_template: all 6 templates load ─────────────

    #[test]
    fn load_prompt_template_all_templates() {
        for name in &[
            "triage",
            "taskmanager",
            "developer",
            "reviewer",
            "product",
            "qa",
        ] {
            let result = load_prompt_template(name);
            assert!(result.is_ok(), "template '{}' should load", name);
        }
    }

    use std::sync::Arc;

    #[tokio::test]
    async fn shell_deps_returns_shell() {
        let shell = create_test_shell();
        let deps = WorkflowContext {
            config: GitAutomateConfig {
                projects: std::collections::BTreeMap::new(),
            },
            github: None,
            shell: shell.clone(),
        };
        let sd = deps.shell_deps();
        // shell is an Arc<dyn Fn>, just verify it's the same by calling it
        let result = (sd.shell)("echo test".to_string()).await;
        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn context_deps_returns_github_and_config() {
        let shell = create_test_shell();
        let config = GitAutomateConfig {
            projects: std::collections::BTreeMap::new(),
        };
        let deps = WorkflowContext {
            config: config.clone(),
            github: None,
            shell,
        };
        let cd = deps.context_deps();
        assert!(cd.github.is_none());
        assert_eq!(cd.config.projects.len(), 0);
    }

    fn create_test_shell() -> ShellFn {
        Arc::new(|command: String| {
            Box::pin(async move { crate::shell::execute_shell(&command).await })
        })
    }
}

#[cfg(test)]
mod log_capture_test {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use tracing::Subscriber;
    use tracing::field::Visit;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    #[derive(Clone, Default)]
    pub struct SharedBuffer(pub Arc<StdMutex<Vec<String>>>);

    pub struct CapturingLayer {
        buffer: SharedBuffer,
    }

    impl<S> Layer<S> for CapturingLayer
    where
        S: Subscriber,
    {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            let level_str = match *event.metadata().level() {
                tracing::Level::ERROR => "ERROR",
                tracing::Level::WARN => "WARN",
                tracing::Level::INFO => "INFO",
                tracing::Level::DEBUG => "DEBUG",
                tracing::Level::TRACE => "TRACE",
            };

            let mut collector = FieldCollector {
                message: String::new(),
            };
            event.record(&mut collector);

            self.buffer.0.lock().unwrap().push(format!(
                "[git-automate][{}] {}",
                level_str, collector.message
            ));
        }
    }

    struct FieldCollector {
        message: String,
    }

    impl Visit for FieldCollector {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = format!("{:?}", value);
            }
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            }
        }
    }

    pub struct LogCapture {
        buffer: SharedBuffer,
        _guard: tracing::subscriber::DefaultGuard,
    }

    impl LogCapture {
        pub fn install() -> Self {
            let buffer = SharedBuffer::default();
            let layer = CapturingLayer {
                buffer: buffer.clone(),
            };
            let subscriber = tracing_subscriber::registry().with(layer);
            let guard = tracing::subscriber::set_default(subscriber);
            Self {
                buffer,
                _guard: guard,
            }
        }

        pub fn messages(&self) -> Vec<String> {
            self.buffer.0.lock().unwrap().clone()
        }
    }
}

#[cfg(test)]
pub use log_capture_test::LogCapture;
