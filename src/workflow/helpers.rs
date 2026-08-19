//! Shared workflow helpers.
//!
//! Defines: shared types (`ProjectContext`, `FieldIds`, dependency structs),
//! prompt template loading/filling, config writing, and
//! context + field resolution used by every workflow check.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

use regex::Regex;
use serde_json::{Value, json};

use super::WorkflowStatus;
use crate::config::{GitAutomateConfig, GitSection};
use crate::external_issues::github::client::{GitHubClient, GitHubError};
use crate::external_issues::github::repo::parse_repository_url;
use crate::external_issues::github::types::{IssueInfo, ParsedRepo, StatusOption};

// ─── Constants ────────────────────────────────────────────────

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
    pub config: GitSection,
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

/// Dependencies for context resolution (`resolve_context`).
#[derive(Debug, Clone)]
pub struct ContextDeps {
    pub github: Option<GitHubClient>,
    pub config: GitAutomateConfig,
    pub project_id_cache: Arc<Mutex<HashMap<String, String>>>,
}

/// Full dependency set for workflow checks and the orchestrator.
#[derive(Clone)]
pub struct WorkflowContext {
    pub config: GitAutomateConfig,
    pub github: Option<GitHubClient>,
    pub project_id_cache: Arc<Mutex<HashMap<String, String>>>,
}

impl std::fmt::Debug for WorkflowContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowContext")
            .field("config", &self.config)
            .field("github", &self.github)
            .field("project_id_cache", &"<cache>")
            .finish()
    }
}

impl WorkflowContext {
    /// Convenience: extract the context-resolution dependencies.
    pub fn context_deps(&self) -> ContextDeps {
        ContextDeps {
            github: self.github.clone(),
            config: self.config.clone(),
            project_id_cache: self.project_id_cache.clone(),
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

/// Load an embedded agent definition file by short name.
///
/// Mirrors [`load_prompt_template`] but reads from `src/assets/agents/`.
/// Uses `include_str!` — no runtime file access.
///
/// # Errors
/// Returns `WorkflowError::TemplateNotFound` for unknown names.
pub fn load_agent_template(name: &str) -> Result<String, WorkflowError> {
    let content = match name {
        "triage" => include_str!("../assets/agents/git-automate-triage.agent.md"),
        "taskmanager" => include_str!("../assets/agents/git-automate-taskmanager.agent.md"),
        "developer" => include_str!("../assets/agents/git-automate-developer.agent.md"),
        "reviewer" => include_str!("../assets/agents/git-automate-reviewer.agent.md"),
        "product" => include_str!("../assets/agents/git-automate-product.agent.md"),
        "qa" => include_str!("../assets/agents/git-automate-qa.agent.md"),
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

// ─── Session ID extraction ────────────────────────────────────

/// Extract the `sessionId` value from a project field-value map, returning
/// `None` when the key is absent or not a string.
///
/// Equivalent to the repeated pattern:
/// `map.get("sessionId").and_then(|v| v.as_deref())`.
pub fn extract_session_id(field_values: &BTreeMap<String, Option<String>>) -> Option<String> {
    field_values
        .get("sessionId")
        .and_then(|v| v.as_deref())
        .map(String::from)
}

// ─── Config helpers ───────────────────────────────────────────

/// Read `git-automate.yml`, set `git.projectId`, and write it back to disk —
/// also updating the in-memory config.
///
/// Equivalent to TS `writeProjectId`. Uses `serde_yaml::Value` for
/// round-trip preservation of the YAML structure.
pub async fn write_project_id(
    _project_name: &str,
    project_id: &str,
    config: &mut GitAutomateConfig,
) -> Result<(), WorkflowError> {
    let config_path = crate::config::resolve_config_path()
        .map_err(|e| WorkflowError::ConfigNotFound(e.to_string()))?;
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

    // Ensure "git" exists as a mapping.
    let git_key = serde_yaml::Value::String("git".to_string());
    if !root.contains_key(&git_key) {
        root.insert(
            git_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }

    let git_section = root
        .get_mut(&git_key)
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| WorkflowError::Other("git is not a mapping".to_string()))?;

    git_section.insert(
        serde_yaml::Value::String("projectId".to_string()),
        serde_yaml::Value::String(project_id.to_string()),
    );

    // Write back to disk.
    let yaml_content = serde_yaml::to_string(&data)?;
    std::fs::write(&config_path, yaml_content)?;

    // Keep in-memory config in sync.
    config.git.project_id = Some(project_id.to_string());

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

// ─── Project ID resolution ──────────────────────────────────────

/// Resolve a project ID that may be a numeric project number to a global node ID.
///
/// If `project_id` is purely numeric (a project number / databaseId), resolves
/// it to a relay global ID via [`GitHubClient::get_project_by_number`].
/// Otherwise, treats it as an already-valid global ID and returns it unchanged.
///
/// Returns `(resolved_id, was_resolved)` — `was_resolved` is `true` when the
/// input was numeric and a different global ID was produced, allowing callers
/// to persist the resolved ID back to config.
pub async fn resolve_project_id(
    github: &GitHubClient,
    owner: &str,
    project_id: &str,
) -> Result<(String, bool), WorkflowError> {
    if project_id.chars().all(|c| c.is_ascii_digit()) {
        let number: i64 = project_id.parse().map_err(|e| {
            WorkflowError::Other(format!("invalid project number '{}': {}", project_id, e))
        })?;
        tracing::info!(
            "Resolving project number {} for owner {} to a global ID",
            number,
            owner
        );
        let global_id = github.get_project_by_number(owner, number).await?;
        Ok((global_id, true))
    } else {
        Ok((project_id.to_string(), false))
    }
}

/// Like [`resolve_project_id`] but memoizes the result in *cache*, keyed by
/// `"owner/project_id"`, so repeated calls never hit the network twice.
pub async fn resolve_project_id_cached(
    cache: &Mutex<HashMap<String, String>>,
    github: &GitHubClient,
    owner: &str,
    project_id: &str,
) -> Result<String, WorkflowError> {
    let key = format!("{}/{}", owner, project_id);

    {
        let guard = cache.lock().await;
        if let Some(cached) = guard.get(&key) {
            return Ok(cached.clone());
        }
    }

    let resolved = resolve_project_id(github, owner, project_id).await?.0;
    let mut guard = cache.lock().await;
    guard.insert(key, resolved.clone());
    Ok(resolved)
}

// ─── Context resolution ───────────────────────────────────────

/// Resolve a project name + config into a fully populated [`ProjectContext`],
/// creating the project on GitHub if it does not yet exist.
///
/// Numeric project IDs (e.g. `1`) are resolved to global node IDs at runtime
/// via [`resolve_project_id`] — the original numeric ID is kept in the config
/// file, not replaced by the resolved global ID. New project IDs are persisted
/// to `git-automate.yml` when created.
///
/// Equivalent to TS `resolveContext`.
pub async fn resolve_context(
    deps: &ContextDeps,
    project_name: &str,
    git_section: &GitSection,
) -> Result<ProjectContext, WorkflowError> {
    let github = deps
        .github
        .as_ref()
        .ok_or_else(|| WorkflowError::NoGitHub(project_name.to_string()))?;

    let ParsedRepo { owner, repo } = parse_repository_url(&git_section.repository)
        .map_err(|e| WorkflowError::Other(e.to_string()))?;

    // Numeric project IDs (e.g. "1") are resolved to global node IDs at runtime.
    // The config file is NOT modified — the original numeric ID is preserved
    // so users can keep `projectId: 1` and have it resolved each time.
    let project_id = if let Some(pid) = &git_section.project_id {
        resolve_project_id_cached(&deps.project_id_cache, github, &owner, pid).await?
    } else {
        tracing::info!("Creating project {} for {}/{}", project_name, owner, repo);
        let pid = github.create_project(&owner, project_name).await?;
        let mut config = deps.config.clone();
        write_project_id(project_name, &pid, &mut config).await?;
        pid
    };

    // Ensure Status field options and sessionId field exist so triage/todo/review
    // checks are resilient even without setup_project having run first.
    ensure_status_options(github, &project_id).await?;
    ensure_session_id_field(github, &project_id).await?;

    Ok(ProjectContext {
        name: project_name.to_string(),
        config: git_section.clone(),
        owner,
        repo,
        project_id,
    })
}

/// Ensure the project's "Status" field has all [`WorkflowStatus`] options.
/// Equivalent to the private `Workflow::ensure_status_options` method in `mod.rs`.
pub async fn ensure_status_options(
    github: &GitHubClient,
    project_id: &str,
) -> Result<(), WorkflowError> {
    let status_field = github
        .get_project_status_field(project_id)
        .await?
        .ok_or_else(|| WorkflowError::NoStatusField(project_id.to_string()))?;

    let standard_names: HashSet<&str> = WorkflowStatus::all().iter().map(|s| s.as_str()).collect();

    let kept: Vec<&StatusOption> = status_field
        .options
        .iter()
        .filter(|o| standard_names.contains(o.name.as_str()))
        .collect();

    let kept_names: HashSet<&str> = kept.iter().map(|o| o.name.as_str()).collect();
    let missing: Vec<Value> = WorkflowStatus::all()
        .iter()
        .map(|s| s.as_str())
        .filter(|name| !kept_names.contains(*name))
        .map(|name| json!({ "name": name, "color": "GRAY", "description": "" }))
        .collect();

    let missing_count = missing.len();
    let all_options: Vec<Value> = kept
        .iter()
        .map(|o| json!({ "id": o.id, "name": o.name, "color": "GRAY", "description": "" }))
        .chain(missing)
        .collect();

    if all_options.len() != status_field.options.len() {
        tracing::info!(
            "Updating {} status options (removed {}, added {})",
            all_options.len(),
            status_field.options.len() - kept.len(),
            missing_count,
        );
        github
            .add_project_status_options(&status_field.id, &all_options)
            .await?;
    }
    Ok(())
}

/// Ensure the project has a `sessionId` text field, creating it if missing.
/// Equivalent to the private `Workflow::ensure_session_id_field` method in `mod.rs`.
pub async fn ensure_session_id_field(
    github: &GitHubClient,
    project_id: &str,
) -> Result<(), WorkflowError> {
    let fields = github.get_project_fields(project_id).await?;
    let has_session_id = fields.iter().any(|f| f.name == SESSION_FIELD_NAME);

    if !has_session_id {
        tracing::info!("Adding sessionId field");
        github
            .add_project_field(project_id, SESSION_FIELD_NAME, "TEXT")
            .await?;
    }
    Ok(())
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

pub fn detect_git_remote() -> Option<ParsedRepo> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_repository_url(&url).ok()
}

pub fn write_doctor_config(
    config_path: &Path,
    _project_name: &str,
    repository: &str,
    project_id: &str,
    directory: &str,
) -> Result<(), WorkflowError> {
    let mut data: serde_yaml::Value = if config_path.exists() {
        let content = std::fs::read_to_string(config_path)?;
        serde_yaml::from_str(&content)?
    } else {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    };

    let root = data
        .as_mapping_mut()
        .ok_or_else(|| WorkflowError::Other("config root is not a mapping".to_string()))?;

    // Ensure "git" exists as a mapping.
    let git_key = serde_yaml::Value::String("git".to_string());
    if !root.contains_key(&git_key) {
        root.insert(
            git_key.clone(),
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
        );
    }

    let git_section = root
        .get_mut(&git_key)
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| WorkflowError::Other("git is not a mapping".to_string()))?;

    git_section.insert(
        serde_yaml::Value::String("repository".to_string()),
        serde_yaml::Value::String(repository.to_string()),
    );

    git_section.insert(
        serde_yaml::Value::String("directory".to_string()),
        serde_yaml::Value::String(directory.to_string()),
    );

    let project_id_value = if project_id.chars().all(|c| c.is_ascii_digit()) {
        serde_yaml::Value::Number(project_id.parse().unwrap())
    } else {
        serde_yaml::Value::String(project_id.to_string())
    };
    git_section.insert(
        serde_yaml::Value::String("projectId".to_string()),
        project_id_value,
    );

    let yaml_content = serde_yaml::to_string(&data)?;
    std::fs::write(config_path, yaml_content)?;
    Ok(())
}

pub fn prompt_input(message: &str) -> String {
    print!("{}", message);
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).ok();
    input.trim().to_string()
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

    // ── load_agent_template tests ──────────────────────────────

    // Test 5: load_agent_template("triage") → returns content containing YAML frontmatter
    #[test]
    fn load_agent_template_triage() {
        let content = load_agent_template("triage").expect("triage agent should load");
        assert!(
            content.contains("git-automate-triage"),
            "triage agent should contain its name"
        );
        assert!(
            content.contains("subagent"),
            "triage agent should contain mode: subagent"
        );
    }

    // Test 6: load_agent_template("nonexistent") → Err(TemplateNotFound)
    #[test]
    fn load_agent_template_nonexistent() {
        let result = load_agent_template("nonexistent");
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
        assert_eq!(STATUS_FIELD_NAME, "Status");
        assert_eq!(SESSION_FIELD_NAME, "sessionId");
    }

    // ── write_project_id tests ─────────────────────────────────

    #[tokio::test]
    async fn write_project_id_creates_and_updates() {
        let yaml = "git:\n  repository: https://github.com/user/repo\n  directory: /test-dir\n";
        let tmp_dir = tempfile::tempdir().unwrap();
        let config_path = tmp_dir.path().join("git-automate.yml");
        std::fs::write(&config_path, yaml).unwrap();

        // Load config so the git section is populated.
        let mut config = crate::config::parse_config(&config_path)
            .map_err(|e| format!("parse failed: {}", e))
            .unwrap();
        assert_eq!(config.git.project_id, None);

        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().await;
        let original_dir = env::current_dir().unwrap();
        env::set_current_dir(tmp_dir.path()).unwrap();

        write_project_id("my-repo", "PID-123", &mut config)
            .await
            .unwrap();

        env::set_current_dir(&original_dir).unwrap();

        // Verify in-memory config updated.
        assert_eq!(config.git.project_id.as_deref(), Some("PID-123"));

        // Verify on-disk YAML updated.
        let written = std::fs::read_to_string(&config_path).unwrap();
        let data: serde_yaml::Value = serde_yaml::from_str(&written).unwrap();
        let pid = data
            .get("git")
            .and_then(|g| g.get("projectId"))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(pid, "PID-123");
    }

    #[tokio::test]
    async fn write_project_id_missing_file() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let mut config = GitAutomateConfig {
            git: GitSection::default(),
            opencode: None,
        };

        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().await;
        let original_dir = env::current_dir().unwrap();
        env::set_current_dir(tmp_dir.path()).unwrap();

        let result = write_project_id("ghost", "PID", &mut config).await;

        env::set_current_dir(&original_dir).unwrap();

        assert!(matches!(result, Err(WorkflowError::ConfigNotFound(_))));
    }

    #[tokio::test]
    async fn write_project_id_creates_new_project_entry() {
        let yaml = "git:\n  repository: https://github.com/u/r\n  directory: /test-dir\n";
        let tmp_dir = tempfile::tempdir().unwrap();
        let config_path = tmp_dir.path().join("git-automate.yml");
        std::fs::write(&config_path, yaml).unwrap();

        let mut config = crate::config::parse_config(&config_path)
            .map_err(|e| format!("parse failed: {}", e))
            .unwrap();
        assert_eq!(config.git.project_id, None);

        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().await;
        let original_dir = env::current_dir().unwrap();
        env::set_current_dir(tmp_dir.path()).unwrap();

        write_project_id("new-project", "PID-NEW", &mut config)
            .await
            .unwrap();

        env::set_current_dir(&original_dir).unwrap();

        let written = std::fs::read_to_string(&config_path).unwrap();
        let data: serde_yaml::Value = serde_yaml::from_str(&written).unwrap();
        let pid = data
            .get("git")
            .and_then(|g| g.get("projectId"))
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

    // ── resolve_context / resolve_field_ids / resolve_status_option_and_session tests ──

    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::test_utils::gh_client;

    #[tokio::test]
    async fn resolve_project_id_numeric_resolves_to_global() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": { "projectV2": { "id": "PVT-global-4" } },
                    "organization": null
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (resolved, was_resolved) = resolve_project_id(&client, "owner", "4").await.unwrap();
        assert_eq!(resolved, "PVT-global-4");
        assert!(was_resolved);
        server.verify().await;
    }

    #[tokio::test]
    async fn resolve_project_id_non_numeric_returns_unchanged() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        // No mocks should be hit — non-numeric IDs are returned as-is.
        let (resolved, was_resolved) = resolve_project_id(&client, "owner", "PID-123")
            .await
            .unwrap();
        assert_eq!(resolved, "PID-123");
        assert!(!was_resolved);
        server.verify().await;
    }

    #[tokio::test]
    async fn resolve_project_id_not_found_returns_error() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": { "projectV2": null },
                    "organization": { "projectV2": null }
                }
            })))
            .mount(&server)
            .await;

        let result = resolve_project_id(&client, "owner", "999").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn resolve_project_id_cached_hits_network_once() {
        let server = MockServer::start().await;
        let client = gh_client(&server);
        let cache = Arc::new(Mutex::new(HashMap::new()));

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": { "projectV2": { "id": "PVT-global-4" } },
                    "organization": null
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let first = resolve_project_id_cached(&cache, &client, "owner", "4")
            .await
            .unwrap();
        let second = resolve_project_id_cached(&cache, &client, "owner", "4")
            .await
            .unwrap();

        assert_eq!(first, "PVT-global-4");
        assert_eq!(second, "PVT-global-4");
        assert_eq!(first, second);
        server.verify().await;
    }

    #[tokio::test]
    async fn resolve_project_id_cached_different_keys_independent() {
        let server_a = MockServer::start().await;
        let server_b = MockServer::start().await;
        let cache = Arc::new(Mutex::new(HashMap::new()));

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": { "projectV2": { "id": "PVT-A" } },
                    "organization": null
                }
            })))
            .expect(1)
            .mount(&server_a)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "organization": { "projectV2": { "id": "PVT-B" } },
                    "user": null
                }
            })))
            .expect(2)
            .mount(&server_b)
            .await;

        let a = resolve_project_id_cached(&cache, &gh_client(&server_a), "owner", "1")
            .await
            .unwrap();
        let b = resolve_project_id_cached(&cache, &gh_client(&server_b), "other", "2")
            .await
            .unwrap();

        assert_eq!(a, "PVT-A");
        assert_eq!(b, "PVT-B");
        server_a.verify().await;
        server_b.verify().await;
    }

    fn test_git_section(project_id: Option<&str>) -> GitSection {
        GitSection {
            repository: "https://github.com/owner/repo".to_string(),
            project_id: project_id.map(String::from),
            directory: "/test-work".to_string(),
            issue_provider: "github".to_string(),
            title_pattern: "@ai.*".to_string(),
            trello_api_key: None,
            trello_token: None,
            trello_board_id: None,
            token: None,
        }
    }

    fn test_config(project_id: Option<&str>) -> GitAutomateConfig {
        let git = test_git_section(project_id);
        GitAutomateConfig {
            git,
            opencode: None,
        }
    }

    #[tokio::test]
    async fn resolve_context_with_existing_project_id() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id":"o1","name":"Triage"},{"id":"o2","name":"Todo"},
                                {"id":"o3","name":"In Development"},{"id":"o4","name":"Review Technical"},
                                {"id":"o5","name":"Review Product"},{"id":"o6","name":"QA"},{"id":"o7","name":"Done"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id":"f1","name":"Status","dataType":"SINGLE_SELECT"},
                                {"id":"f2","name":"sessionId","dataType":"TEXT"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2Field"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2Field"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let deps = ContextDeps {
            github: Some(client),
            config: test_config(Some("PID-123")),
            project_id_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        let git_section = test_git_section(Some("PID-123"));

        let ctx = resolve_context(&deps, "test-project", &git_section)
            .await
            .expect("resolve_context should succeed with existing projectId");
        assert_eq!(ctx.name, "test-project");
        assert_eq!(ctx.owner, "owner");
        assert_eq!(ctx.repo, "repo");
        assert_eq!(ctx.project_id, "PID-123");
        server.verify().await;
    }

    #[tokio::test]
    async fn resolve_context_numeric_project_id_resolves_and_does_not_persist() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("user(login:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": { "projectV2": { "id": "PVT-global-1" } }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id":"o1","name":"Triage"},{"id":"o2","name":"Todo"},
                                {"id":"o3","name":"In Development"},{"id":"o4","name":"Review Technical"},
                                {"id":"o5","name":"Review Product"},{"id":"o6","name":"QA"},{"id":"o7","name":"Done"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id":"f1","name":"Status","dataType":"SINGLE_SELECT"},
                                {"id":"f2","name":"sessionId","dataType":"TEXT"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let deps = ContextDeps {
            github: Some(client),
            config: test_config(Some("1")),
            project_id_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        let git_section = test_git_section(Some("1"));

        let tmp_dir = tempfile::tempdir().unwrap();
        let config_path = tmp_dir.path().join("git-automate.yml");
        std::fs::write(
            &config_path,
            "git:\n  repository: https://github.com/owner/repo\n  projectId: 1\n",
        )
        .unwrap();

        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().await;
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp_dir.path()).unwrap();

        let ctx = resolve_context(&deps, "test-project", &git_section)
            .await
            .expect("resolve_context should succeed");

        std::env::set_current_dir(&original_dir).unwrap();

        assert_eq!(ctx.project_id, "PVT-global-1");
        assert_eq!(ctx.owner, "owner");
        assert_eq!(ctx.repo, "repo");

        let written = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            written.contains("projectId: 1"),
            "config should still have projectId: 1"
        );
        assert!(
            !written.contains("PVT-global-1"),
            "config should not contain the resolved global ID"
        );

        server.verify().await;
    }

    #[tokio::test]
    async fn resolve_context_without_project_id_creates_and_writes() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        // body_string_contains differentiates the get_owner_id query and createProjectV2 mutation
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("user(login:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "user": { "id": "U123" }, "organization": null }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "createProjectV2": { "id": "NEW_PID" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "sf",
                            "options": [
                                {"id":"o1","name":"Triage"},{"id":"o2","name":"Todo"},
                                {"id":"o3","name":"In Development"},{"id":"o4","name":"Review Technical"},
                                {"id":"o5","name":"Review Product"},{"id":"o6","name":"QA"},{"id":"o7","name":"Done"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id":"f1","name":"Status","dataType":"SINGLE_SELECT"},
                                {"id":"f2","name":"sessionId","dataType":"TEXT"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let deps = ContextDeps {
            github: Some(client),
            config: test_config(None),
            project_id_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        let git_section = test_git_section(None);

        let tmp_dir = tempfile::tempdir().unwrap();
        let config_path = tmp_dir.path().join("git-automate.yml");
        std::fs::write(
            &config_path,
            "git:\n  repository: https://github.com/owner/repo\n",
        )
        .unwrap();

        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().await;
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp_dir.path()).unwrap();

        let ctx = resolve_context(&deps, "test-project", &git_section)
            .await
            .expect("resolve_context should succeed");

        std::env::set_current_dir(&original_dir).unwrap();

        assert_eq!(ctx.project_id, "NEW_PID");
        assert_eq!(ctx.owner, "owner");
        assert_eq!(ctx.repo, "repo");

        let written = std::fs::read_to_string(&config_path).unwrap();
        let data: serde_yaml::Value = serde_yaml::from_str(&written).unwrap();
        let pid = data
            .get("git")
            .and_then(|g| g.get("projectId"))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(pid, "NEW_PID");
    }

    #[tokio::test]
    async fn resolve_context_no_github_returns_error() {
        let deps = ContextDeps {
            github: None,
            config: test_config(Some("PID-123")),
            project_id_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        let result =
            resolve_context(&deps, "test-project", &test_git_section(Some("PID-123"))).await;
        assert!(matches!(result, Err(WorkflowError::NoGitHub(_))));
    }

    #[tokio::test]
    async fn resolve_context_invalid_repository_returns_error() {
        let server = MockServer::start().await;
        let client = gh_client(&server);
        let deps = ContextDeps {
            github: Some(client),
            config: test_config(Some("PID-123")),
            project_id_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        let git_section = GitSection {
            repository: "invalid-no-slash".to_string(),
            project_id: Some("PID-123".to_string()),
            directory: "/test-work".to_string(),
            issue_provider: "github".to_string(),
            title_pattern: "@ai.*".to_string(),
            trello_api_key: None,
            trello_token: None,
            trello_board_id: None,
            token: None,
        };

        let result = resolve_context(&deps, "test-project", &git_section).await;
        assert!(matches!(result, Err(WorkflowError::Other(_))));
    }

    // ── ensure_status_options standalone function tests ───────────

    #[tokio::test]
    async fn ensure_status_options_all_present_no_add() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "sf",
                            "options": [
                                {"id":"o1","name":"Triage"},{"id":"o2","name":"Todo"},
                                {"id":"o3","name":"In Development"},{"id":"o4","name":"Review Technical"},
                                {"id":"o5","name":"Review Product"},{"id":"o6","name":"QA"},{"id":"o7","name":"Done"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2Field"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let result = ensure_status_options(&client, "PID-123").await;
        assert!(result.is_ok());
        server.verify().await;
    }

    #[tokio::test]
    async fn ensure_status_options_missing_calls_add() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "sf",
                            "options": [
                                {"id":"o7","name":"Done"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2Field"))
            .and(body_string_contains("... on ProjectV2Field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "updateProjectV2Field": { "projectV2Field": { "id": "x" } } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let result = ensure_status_options(&client, "PID-123").await;
        assert!(result.is_ok());
        server.verify().await;
    }

    #[tokio::test]
    async fn ensure_status_options_no_field_returns_error() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "field": null } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let result = ensure_status_options(&client, "PID-123").await;
        assert!(matches!(result, Err(WorkflowError::NoStatusField(_))));
        server.verify().await;
    }

    // ── ensure_session_id_field standalone function tests ─────────

    #[tokio::test]
    async fn ensure_session_id_field_exists_no_add() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id":"f1","name":"Status","dataType":"SINGLE_SELECT"},
                                {"id":"f2","name":"sessionId","dataType":"TEXT"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2Field"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let result = ensure_session_id_field(&client, "PID-123").await;
        assert!(result.is_ok());
        server.verify().await;
    }

    #[tokio::test]
    async fn ensure_session_id_field_missing_calls_add() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id":"f1","name":"Status","dataType":"SINGLE_SELECT"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2Field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "createProjectV2Field": { "projectField": { "id": "field-id" } } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let result = ensure_session_id_field(&client, "PID-123").await;
        assert!(result.is_ok());
        server.verify().await;
    }

    // ── resolve_context new behavior tests ────────────────────────

    #[tokio::test]
    async fn resolve_context_creates_status_options_when_missing() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "sf",
                            "options": [
                                {"id":"o7","name":"Done"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id":"f1","name":"Status","dataType":"SINGLE_SELECT"},
                                {"id":"f2","name":"sessionId","dataType":"TEXT"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2Field"))
            .and(body_string_contains("... on ProjectV2Field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "updateProjectV2Field": { "projectV2Field": { "id": "x" } } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2Field"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        let deps = ContextDeps {
            github: Some(client),
            config: test_config(Some("PID-123")),
            project_id_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        let git_section = test_git_section(Some("PID-123"));

        let ctx = resolve_context(&deps, "test-project", &git_section)
            .await
            .expect("resolve_context should succeed");
        assert_eq!(ctx.project_id, "PID-123");
        server.verify().await;
    }

    #[tokio::test]
    async fn resolve_context_creates_session_field_when_missing() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "sf",
                            "options": [
                                {"id":"o1","name":"Triage"},{"id":"o2","name":"Todo"},
                                {"id":"o3","name":"In Development"},{"id":"o4","name":"Review Technical"},
                                {"id":"o5","name":"Review Product"},{"id":"o6","name":"QA"},{"id":"o7","name":"Done"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id":"f1","name":"Status","dataType":"SINGLE_SELECT"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2Field"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2Field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "createProjectV2Field": { "projectField": { "id": "field-id" } } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let deps = ContextDeps {
            github: Some(client),
            config: test_config(Some("PID-123")),
            project_id_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        let git_section = test_git_section(Some("PID-123"));

        let ctx = resolve_context(&deps, "test-project", &git_section)
            .await
            .expect("resolve_context should succeed");
        assert_eq!(ctx.project_id, "PID-123");
        server.verify().await;
    }

    #[tokio::test]
    async fn resolve_field_ids_resolves_correctly() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        // body_string_contains differentiates get_project_status_field and get_project_fields queries
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "o1", "name": "Triage"},
                                {"id": "o2", "name": "Todo"}
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"}
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let ids = resolve_field_ids(&client, "PID-123")
            .await
            .expect("resolve_field_ids should succeed");
        assert_eq!(ids.status_field_id, "status-field-id");
        assert_eq!(ids.session_field_id.as_deref(), Some("session-field-id"));
    }

    #[tokio::test]
    async fn resolve_field_ids_no_status_field_returns_error() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": { "node": { "field": null } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let result = resolve_field_ids(&client, "PID-123").await;
        assert!(matches!(result, Err(WorkflowError::NoStatusField(_))));
    }

    #[tokio::test]
    async fn resolve_field_ids_no_session_field_returns_none() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [{"id": "o1", "name": "Triage"}]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"}
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let ids = resolve_field_ids(&client, "PID-123")
            .await
            .expect("resolve_field_ids should succeed");
        assert_eq!(ids.status_field_id, "status-field-id");
        assert!(ids.session_field_id.is_none());
    }

    #[tokio::test]
    async fn resolve_status_option_and_session_extracts_correctly() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "triage-opt-id", "name": "Triage"},
                                {"id": "todo-opt-id", "name": "Todo"}
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"},
                                {"id": "session-field-id", "name": "sessionId", "dataType": "TEXT"}
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (status_field_id, option_id, session_field_id) =
            resolve_status_option_and_session(&client, "PID-123", "Triage")
                .await
                .expect("resolve_status_option_and_session should succeed");
        assert_eq!(status_field_id, "status-field-id");
        assert_eq!(option_id, "triage-opt-id");
        assert_eq!(session_field_id, "session-field-id");
    }

    #[tokio::test]
    async fn resolve_status_option_and_session_option_not_found() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [
                                {"id": "triage-opt-id", "name": "Triage"},
                                {"id": "todo-opt-id", "name": "Todo"}
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let result = resolve_status_option_and_session(&client, "PID-123", "Done").await;
        assert!(matches!(
            result,
            Err(WorkflowError::StatusOptionNotFound(_))
        ));
    }

    #[tokio::test]
    async fn resolve_status_option_and_session_no_session_field() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "field": {
                            "id": "status-field-id",
                            "options": [{"id": "triage-opt-id", "name": "Triage"}]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "fields": {
                            "nodes": [
                                {"id": "status-field-id", "name": "Status", "dataType": "SINGLE_SELECT"}
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let result = resolve_status_option_and_session(&client, "PID-123", "Triage").await;
        assert!(matches!(result, Err(WorkflowError::NoSessionField(_))));
    }

    #[tokio::test]
    async fn resolve_field_ids_github_http_error() {
        let server = MockServer::start().await;
        let client = gh_client(&server);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(500).set_body_json(json!({
                "message": "Internal Server Error"
            })))
            .expect(4)
            .mount(&server)
            .await;

        let result = resolve_field_ids(&client, "PID-123").await;
        assert!(matches!(result.unwrap_err(), WorkflowError::GitHub(_)));
    }

    use std::sync::Arc;

    #[tokio::test]
    async fn context_deps_returns_github_and_config() {
        let config = GitAutomateConfig {
            git: GitSection::default(),
            opencode: None,
        };
        let deps = WorkflowContext {
            config: config.clone(),
            github: None,
            project_id_cache: Arc::new(Mutex::new(HashMap::new())),
        };
        let cd = deps.context_deps();
        assert!(cd.github.is_none());
        assert!(cd.config.git.repository.is_empty());
    }
}
