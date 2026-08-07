//! Workflow orchestrator.
//!
//! The [`Workflow`] struct ties together all workflow checks in sequence:
//! setup → opencode → triage → todo → review. Each public `run_*_check`
//! method wraps per-project work in error isolation (errors are logged,
//! never propagated).

pub mod checks;
pub mod helpers;

use crate::config::ProjectConfig;
use crate::external_agent::opencode::AgentInfo;
use crate::external_agent::opencode::OpenCodeClient;
use crate::external_issues::github::repo::parse_repository_url;
use crate::external_issues::github::types::ParsedRepo;

use self::checks::{OpencodeSessionConfig, run_review_check, run_todo_check, run_triage_check};
use self::helpers::{
    WorkflowContext, WorkflowError, load_agent_template, resolve_context, write_project_id,
};

// ─── Constants ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum AgentName {
    Triage,
    TaskManager,
    Developer,
    Reviewer,
    Product,
    QA,
}

impl AgentName {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentName::Triage => "git-automate-triage",
            AgentName::TaskManager => "git-automate-taskmanager",
            AgentName::Developer => "git-automate-developer",
            AgentName::Reviewer => "git-automate-reviewer",
            AgentName::Product => "git-automate-product",
            AgentName::QA => "git-automate-qa",
        }
    }

    /// Return the full agent definition filename, e.g.
    /// `git-automate-triage.agent.md`.
    pub fn as_file_name(&self) -> &'static str {
        match self {
            AgentName::Triage => "git-automate-triage.agent.md",
            AgentName::TaskManager => "git-automate-taskmanager.agent.md",
            AgentName::Developer => "git-automate-developer.agent.md",
            AgentName::Reviewer => "git-automate-reviewer.agent.md",
            AgentName::Product => "git-automate-product.agent.md",
            AgentName::QA => "git-automate-qa.agent.md",
        }
    }

    pub fn as_template_name(&self) -> &'static str {
        match self {
            AgentName::Triage => "triage",
            AgentName::TaskManager => "taskmanager",
            AgentName::Developer => "developer",
            AgentName::Reviewer => "reviewer",
            AgentName::Product => "product",
            AgentName::QA => "qa",
        }
    }
}

/// The six required OpenCode agents that must be installed.
pub const REQUIRED_AGENTS: [AgentName; 6] = [
    AgentName::Triage,
    AgentName::TaskManager,
    AgentName::Developer,
    AgentName::Reviewer,
    AgentName::Product,
    AgentName::QA,
];

/// The seven workflow status options.
#[derive(Debug, Clone, Copy)]
pub enum WorkflowStatus {
    Triage,
    Todo,
    InDevelopment,
    ReviewTechnical,
    ReviewProduct,
    QA,
    Done,
}

impl WorkflowStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkflowStatus::Triage => "Triage",
            WorkflowStatus::Todo => "Todo",
            WorkflowStatus::InDevelopment => "In Development",
            WorkflowStatus::ReviewTechnical => "Review Technical",
            WorkflowStatus::ReviewProduct => "Review Product",
            WorkflowStatus::QA => "QA",
            WorkflowStatus::Done => "Done",
        }
    }

    pub fn all() -> [WorkflowStatus; 7] {
        [
            WorkflowStatus::Triage,
            WorkflowStatus::Todo,
            WorkflowStatus::InDevelopment,
            WorkflowStatus::ReviewTechnical,
            WorkflowStatus::ReviewProduct,
            WorkflowStatus::QA,
            WorkflowStatus::Done,
        ]
    }
}

// ─── Workflow ──────────────────────────────────────────────────

/// Orchestrator that runs workflow checks in sequence.
pub struct Workflow {
    deps: WorkflowContext,
}

/// A single step in the workflow sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStep {
    Setup,
    OpencodeCheck,
    Triage,
    Todo,
    Review,
}

impl WorkflowStep {
    /// All workflow steps in execution order.
    pub fn all() -> [WorkflowStep; 5] {
        [
            WorkflowStep::Setup,
            WorkflowStep::OpencodeCheck,
            WorkflowStep::Triage,
            WorkflowStep::Todo,
            WorkflowStep::Review,
        ]
    }

    /// Run this step against the given workflow.
    pub async fn run(&self, workflow: &Workflow) -> Result<(), WorkflowError> {
        match self {
            WorkflowStep::Setup => workflow.run_setup_check().await,
            WorkflowStep::OpencodeCheck => workflow.run_opencode_check().await,
            WorkflowStep::Triage => workflow.run_triage_check().await,
            WorkflowStep::Todo => workflow.run_todo_check().await,
            WorkflowStep::Review => workflow.run_review_check().await,
        }
    }
}

impl Workflow {
    /// Create a new `Workflow` with the given dependencies.
    pub fn new(deps: WorkflowContext) -> Self {
        Self { deps }
    }

    // ── Public API ─────────────────────────────────────────────

    /// Run all checks in order: setup → opencode → triage → todo → review.
    ///
    /// Each sub-check catches and logs its own errors internally, so this
    /// method always returns `Ok(())`.
    pub async fn run_all(&self) -> Result<(), WorkflowError> {
        for step in WorkflowStep::all() {
            let _ = step.run(self).await;
        }
        Ok(())
    }

    /// For each project: parse the repo URL, create the GitHub project if
    /// missing, ensure status options, and ensure the sessionId field.
    pub async fn run_setup_check(&self) -> Result<(), WorkflowError> {
        let Some(_github) = self.deps.github.as_ref() else {
            tracing::warn!("GitHub client not available — skipping setup check");
            return Ok(());
        };

        for (project_name, project_config) in &self.deps.config.projects {
            if let Err(e) = self.setup_project(project_name, project_config).await {
                tracing::error!("Setup check failed for {}: {}", project_name, e);
            }
        }
        Ok(())
    }

    /// For each project with an OpenCode config: check server health and
    /// verify required agents are present.
    pub async fn run_opencode_check(&self) -> Result<(), WorkflowError> {
        for (project_name, project_config) in &self.deps.config.projects {
            if project_config.opencode.is_none() {
                continue;
            }
            let oc = self.opencode_config(project_config);
            if let Err(e) = self.check_opencode(&oc).await {
                tracing::error!("OpenCode check failed for {}: {}", project_name, e);
            }
        }
        Ok(())
    }

    /// For each project with an OpenCode config: resolve context, then run
    /// the triage check (`@ai` issues → project item → triage session).
    pub async fn run_triage_check(&self) -> Result<(), WorkflowError> {
        let Some(_github) = self.deps.github.as_ref() else {
            tracing::warn!("GitHub client not available — skipping triage check");
            return Ok(());
        };

        for (project_name, project_config) in &self.deps.config.projects {
            if project_config.opencode.is_none() {
                continue;
            }
            let result = async {
                let ctx = resolve_context(&self.deps.context_deps(), project_name, project_config)
                    .await?;
                let oc = self.opencode_config(project_config);
                run_triage_check(&self.deps, &ctx, &oc).await
            }
            .await;
            if let Err(e) = result {
                tracing::error!("Triage check failed for {}: {}", project_name, e);
            }
        }
        Ok(())
    }

    /// For each project with an OpenCode config: resolve context, then run
    /// the todo check (Todo items → developer session).
    pub async fn run_todo_check(&self) -> Result<(), WorkflowError> {
        let Some(_github) = self.deps.github.as_ref() else {
            tracing::warn!("GitHub client not available — skipping todo check");
            return Ok(());
        };

        for (project_name, project_config) in &self.deps.config.projects {
            if project_config.opencode.is_none() {
                continue;
            }
            let result = async {
                let ctx = resolve_context(&self.deps.context_deps(), project_name, project_config)
                    .await?;
                let oc = self.opencode_config(project_config);
                run_todo_check(&self.deps, &ctx, &oc).await
            }
            .await;
            if let Err(e) = result {
                tracing::error!("Todo check failed for {}: {}", project_name, e);
            }
        }
        Ok(())
    }

    /// For each project with an OpenCode config: resolve context, then run
    /// the review check (Review Technical / Review Product / QA → session).
    pub async fn run_review_check(&self) -> Result<(), WorkflowError> {
        let Some(_github) = self.deps.github.as_ref() else {
            tracing::warn!("GitHub client not available — skipping review check");
            return Ok(());
        };

        for (project_name, project_config) in &self.deps.config.projects {
            if project_config.opencode.is_none() {
                continue;
            }
            let result = async {
                let ctx = resolve_context(&self.deps.context_deps(), project_name, project_config)
                    .await?;
                let oc = self.opencode_config(project_config);
                run_review_check(&self.deps, &ctx, &oc).await
            }
            .await;
            if let Err(e) = result {
                tracing::error!("Review check failed for {}: {}", project_name, e);
            }
        }
        Ok(())
    }

    pub async fn run_doctor_check(&self) -> Result<(), WorkflowError> {
        let _ = self.run_setup_check().await;

        for (project_name, project_config) in &self.deps.config.projects {
            if project_config.opencode.is_none() {
                continue;
            }
            let oc = self.opencode_config(project_config);
            if let Err(e) = self.check_agents_and_create_if_missing(&oc).await {
                tracing::error!("Doctor check failed for {}: {}", project_name, e);
            }
        }
        Ok(())
    }

    // ── Helpers ─────────────────────────────────────────────────

    fn opencode_config(&self, project: &ProjectConfig) -> OpencodeSessionConfig {
        let opencode = project.opencode.as_ref().expect("opencode config present");
        OpencodeSessionConfig {
            url: opencode.url.clone(),
            pw: opencode.pw.clone(),
            directory: project.directory.clone(),
        }
    }

    // ── Private check implementations ─────────────────────────

    /// Parse the repository URL, create the GitHub project if it doesn't
    /// exist yet, then ensure status options and the sessionId field.
    async fn setup_project(&self, name: &str, config: &ProjectConfig) -> Result<(), WorkflowError> {
        let ParsedRepo { owner, repo } = parse_repository_url(&config.repository)
            .map_err(|e| WorkflowError::Other(e.to_string()))?;

        let project_id = if let Some(pid) = &config.project_id {
            pid.clone()
        } else {
            tracing::info!("Creating project {} for {}/{}", name, owner, repo);
            let github = self
                .deps
                .github
                .as_ref()
                .ok_or_else(|| WorkflowError::NoGitHub(name.to_string()))?;
            let pid = github.create_project(&owner, name).await?;
            let mut config_clone = self.deps.config.clone();
            write_project_id(name, &pid, &mut config_clone).await?;
            pid
        };

        self.ensure_status_options(&project_id).await?;
        self.ensure_session_id_field(&project_id).await?;
        Ok(())
    }

    /// Ensure the project's "Status" field has all [`WorkflowStatus`].
    ///
    /// Returns `WorkflowError::NoStatusField` if the field doesn't exist.
    async fn ensure_status_options(&self, project_id: &str) -> Result<(), WorkflowError> {
        let github = self
            .deps
            .github
            .as_ref()
            .ok_or_else(|| WorkflowError::NoGitHub(project_id.to_string()))?;
        helpers::ensure_status_options(github, project_id).await
    }

    /// Ensure the project has a `sessionId` text field.
    async fn ensure_session_id_field(&self, project_id: &str) -> Result<(), WorkflowError> {
        let github = self
            .deps
            .github
            .as_ref()
            .ok_or_else(|| WorkflowError::NoGitHub(project_id.to_string()))?;
        helpers::ensure_session_id_field(github, project_id).await
    }

    /// Check OpenCode server health and verify all required agents exist.
    async fn check_opencode(&self, oc: &OpencodeSessionConfig) -> Result<(), WorkflowError> {
        let client = OpenCodeClient::new(oc.url.clone(), oc.pw.clone());

        if !client.check_health().await {
            tracing::warn!("OpenCode server at {} is not healthy", oc.url);
            return Ok(());
        }

        tracing::info!("OpenCode server at {} is healthy", oc.url);

        let agents: Vec<AgentInfo> = client
            .get_agents(oc.directory.as_deref())
            .await
            .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))?;

        let agent_names: std::collections::HashSet<&str> =
            agents.iter().map(|a| a.name.as_str()).collect();

        let missing: Vec<&str> = REQUIRED_AGENTS
            .iter()
            .map(|a| a.as_str())
            .filter(|a| !agent_names.contains(a))
            .collect();

        if !missing.is_empty() {
            tracing::warn!("Missing required agents: {}", missing.join(", "));
        } else {
            tracing::info!("All required agents present");
        }
        Ok(())
    }

    async fn check_agents_and_create_if_missing(
        &self,
        oc: &OpencodeSessionConfig,
    ) -> Result<(), WorkflowError> {
        let client = OpenCodeClient::new(oc.url.clone(), oc.pw.clone());

        if !client.check_health().await {
            tracing::warn!(
                "OpenCode server at {} is not healthy — skipping agent copy",
                oc.url
            );
            return Ok(());
        }

        tracing::info!("OpenCode server at {} is healthy", oc.url);

        let agents: Vec<AgentInfo> = client
            .get_agents(oc.directory.as_deref())
            .await
            .map_err(|e| WorkflowError::Other(format!("OpenCode: {}", e)))?;

        let agent_names: std::collections::HashSet<&str> =
            agents.iter().map(|a| a.name.as_str()).collect();

        let missing: Vec<&AgentName> = REQUIRED_AGENTS
            .iter()
            .filter(|a| !agent_names.contains(a.as_str()))
            .collect();

        if missing.is_empty() {
            tracing::info!("All required agents present");
            return Ok(());
        }

        tracing::info!(
            "Missing required agents: {}",
            missing
                .iter()
                .map(|a| a.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );

        let home = match std::env::var("HOME") {
            Ok(h) => h,
            Err(_) => {
                tracing::warn!("HOME environment variable not set — cannot copy agents");
                return Ok(());
            }
        };

        let agents_dir = std::path::Path::new(&home).join(".opencode").join("agents");

        std::fs::create_dir_all(&agents_dir)
            .map_err(|e| WorkflowError::Other(format!("Failed to create agents dir: {}", e)))?;

        for agent in &missing {
            let content = load_agent_template(agent.as_template_name())?;
            let dest = agents_dir.join(agent.as_file_name());
            std::fs::write(&dest, &content).map_err(|e| {
                WorkflowError::Other(format!("Failed to write agent {}: {}", agent.as_str(), e))
            })?;
            tracing::info!("Copied agent {} to {}", agent.as_str(), dest.display());
        }

        Ok(())
    }
}

// ─── Tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GitAutomateConfig, OpencodeConfig, ProjectConfig};
    use crate::test_utils::{gh_client, make_deps, mock_shell};
    use crate::workflow::helpers::LogCapture;
    use std::collections::BTreeMap;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── Test helpers ──────────────────────────────────────────

    /// Build a project config without opencode.
    fn make_project_no_opencode() -> ProjectConfig {
        ProjectConfig {
            repository: "https://github.com/owner/repo".to_string(),
            project_id: Some("PID-123".to_string()),
            directory: None,
            opencode: None,
            issue_provider: "github".to_string(),
            title_pattern: "@ai.*".to_string(),
            trello_api_key: None,
            trello_token: None,
            trello_board_id: None,
        }
    }

    // ── Constants tests ───────────────────────────────────────

    #[test]
    fn required_agents_matches_ts() {
        assert_eq!(REQUIRED_AGENTS.len(), 6);
        assert_eq!(REQUIRED_AGENTS[0].as_str(), "git-automate-triage");
        assert_eq!(REQUIRED_AGENTS[1].as_str(), "git-automate-taskmanager");
        assert_eq!(REQUIRED_AGENTS[2].as_str(), "git-automate-developer");
        assert_eq!(REQUIRED_AGENTS[3].as_str(), "git-automate-reviewer");
        assert_eq!(REQUIRED_AGENTS[4].as_str(), "git-automate-product");
        assert_eq!(REQUIRED_AGENTS[5].as_str(), "git-automate-qa");
    }

    #[test]
    fn status_options_matches_ts() {
        let statuses = WorkflowStatus::all();
        assert_eq!(statuses.len(), 7);
        assert_eq!(
            statuses.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            [
                "Triage",
                "Todo",
                "In Development",
                "Review Technical",
                "Review Product",
                "QA",
                "Done"
            ]
        );
    }

    #[test]
    fn agent_file_name_all_variants() {
        assert_eq!(
            REQUIRED_AGENTS[0].as_file_name(),
            "git-automate-triage.agent.md"
        );
        assert_eq!(
            REQUIRED_AGENTS[1].as_file_name(),
            "git-automate-taskmanager.agent.md"
        );
        assert_eq!(
            REQUIRED_AGENTS[2].as_file_name(),
            "git-automate-developer.agent.md"
        );
        assert_eq!(
            REQUIRED_AGENTS[3].as_file_name(),
            "git-automate-reviewer.agent.md"
        );
        assert_eq!(
            REQUIRED_AGENTS[4].as_file_name(),
            "git-automate-product.agent.md"
        );
        assert_eq!(
            REQUIRED_AGENTS[5].as_file_name(),
            "git-automate-qa.agent.md"
        );
    }

    #[tokio::test]
    async fn check_agents_and_create_if_missing_writes_files() {
        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let saved_home = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let oc_mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "healthy": true,
                "version": "1.0.0"
            })))
            .expect(1)
            .named("health")
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"name": "git-automate-triage", "description": "t", "mode": "subagent", "builtIn": true},
                {"name": "git-automate-taskmanager", "description": "t", "mode": "subagent", "builtIn": true},
                {"name": "git-automate-developer", "description": "t", "mode": "subagent", "builtIn": true},
            ])))
            .expect(1)
            .named("agents")
            .mount(&oc_mock)
            .await;

        let deps = WorkflowContext {
            config: GitAutomateConfig {
                projects: BTreeMap::new(),
                concurrency: None,
            },
            github: None,
            shell: mock_shell(),
        };
        let workflow = Workflow::new(deps);

        let oc = OpencodeSessionConfig {
            url: oc_mock.uri(),
            pw: "test-pw".to_string(),
            directory: None,
        };

        let result = workflow.check_agents_and_create_if_missing(&oc).await;
        assert!(
            result.is_ok(),
            "check_agents_and_create_if_missing should succeed: {:?}",
            result.err()
        );

        let agents_dir = tmp.path().join(".opencode").join("agents");
        assert!(agents_dir.join("git-automate-reviewer.agent.md").exists());
        assert!(agents_dir.join("git-automate-product.agent.md").exists());
        assert!(agents_dir.join("git-automate-qa.agent.md").exists());

        oc_mock.verify().await;

        if let Some(val) = saved_home {
            unsafe {
                std::env::set_var("HOME", val);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
    }

    #[tokio::test]
    async fn run_doctor_check_copies_missing_agents() {
        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().await;
        let tmp = tempfile::tempdir().expect("tempdir");
        let saved_home = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let oc_mock = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "healthy": true,
                "version": "1.0.0"
            })))
            .expect(1)
            .named("health")
            .mount(&oc_mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"name": "git-automate-triage", "description": "t", "mode": "subagent", "builtIn": true},
                {"name": "git-automate-taskmanager", "description": "t", "mode": "subagent", "builtIn": true},
                {"name": "git-automate-developer", "description": "t", "mode": "subagent", "builtIn": true},
            ])))
            .expect(1)
            .named("agents")
            .mount(&oc_mock)
            .await;

        let mut projects = BTreeMap::new();
        projects.insert(
            "test-proj".to_string(),
            ProjectConfig {
                repository: "https://github.com/owner/repo".to_string(),
                project_id: Some("PID-123".to_string()),
                directory: None,
                opencode: Some(OpencodeConfig {
                    url: oc_mock.uri(),
                    pw: "test-pw".to_string(),
                }),
                issue_provider: "github".to_string(),
                title_pattern: "@ai.*".to_string(),
                trello_api_key: None,
                trello_token: None,
                trello_board_id: None,
            },
        );

        let deps = WorkflowContext {
            config: GitAutomateConfig {
                projects,
                concurrency: None,
            },
            github: None,
            shell: mock_shell(),
        };
        let workflow = Workflow::new(deps);

        let result = workflow.run_doctor_check().await;
        assert!(
            result.is_ok(),
            "run_doctor_check should succeed: {:?}",
            result.err()
        );

        let agents_dir = tmp.path().join(".opencode").join("agents");
        assert!(agents_dir.join("git-automate-reviewer.agent.md").exists());
        assert!(agents_dir.join("git-automate-product.agent.md").exists());
        assert!(agents_dir.join("git-automate-qa.agent.md").exists());

        oc_mock.verify().await;

        if let Some(val) = saved_home {
            unsafe {
                std::env::set_var("HOME", val);
            }
        } else {
            unsafe {
                std::env::remove_var("HOME");
            }
        }
    }

    // ── run_all ordering (test 1) ─────────────────────────────

    /// Verify run_all calls all 5 checks in order (via log capture).
    #[tokio::test]
    async fn run_all_calls_all_checks_in_order() {
        // One project with opencode config + github=None → all 5 checks log:
        // setup(skip), opencode(not healthy), triage(skip), todo(skip), review(skip).
        let oc_mock = MockServer::start().await;

        let project = ProjectConfig {
            repository: "https://github.com/owner/repo".to_string(),
            project_id: Some("PID-123".to_string()),
            directory: None,
            opencode: Some(OpencodeConfig {
                url: oc_mock.uri(),
                pw: "test-pw".to_string(),
            }),
            issue_provider: "github".to_string(),
            title_pattern: "@ai.*".to_string(),
            trello_api_key: None,
            trello_token: None,
            trello_board_id: None,
        };
        let mut projects = BTreeMap::new();
        projects.insert("test-proj".to_string(), project);

        let capture = LogCapture::install();
        let deps = WorkflowContext {
            config: GitAutomateConfig {
                projects,
                concurrency: None,
            },
            github: None,
            shell: mock_shell(),
        };

        let workflow = Workflow::new(deps);
        workflow.run_all().await.unwrap();

        let msgs = capture.messages();
        let filtered: Vec<&String> = msgs
            .iter()
            .filter(|m| {
                m.contains("skipping setup check")
                    || m.contains("not healthy")
                    || m.contains("OpenCode server")
                    || m.contains("skipping triage check")
                    || m.contains("skipping todo check")
                    || m.contains("skipping review check")
            })
            .collect();
        assert_eq!(filtered.len(), 5, "expected 5 relevant log messages");
        assert!(filtered[0].contains("skipping setup check"));
        assert!(
            filtered[1].contains("not healthy") || filtered[1].contains("OpenCode server"),
            "expected health-related message, got: {}",
            filtered[1]
        );
        assert!(filtered[2].contains("skipping triage check"));
        assert!(filtered[3].contains("skipping todo check"));
        assert!(filtered[4].contains("skipping review check"));
    }

    // ── run_setup_check with github=None (test 2) ─────────────

    #[tokio::test]
    async fn run_setup_check_with_no_github_logs_warn() {
        let deps = make_deps(None);
        let capture = LogCapture::install();

        let workflow = Workflow::new(deps);
        let result = workflow.run_setup_check().await;

        assert!(result.is_ok());
        let msgs = capture.messages();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("[WARN]"));
        assert!(msgs[0].contains("skipping setup check"));
    }

    // ── run_setup_check with github iterates projects (test 3) ─

    #[tokio::test]
    async fn run_setup_check_with_github_iterates_projects() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);

        // Mock: get_owner_id (login)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "user": { "id": "uid" }, "organization": null }
            })))
            .mount(&mock)
            .await;

        // Mock: createProjectV2 → pid (expect 1)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "createProjectV2": { "id": "new-pid" } }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Mock: status field with all options
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
            .mount(&mock)
            .await;

        // Mock: fields with sessionId present
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
            .mount(&mock)
            .await;

        // Project without projectId → triggers create_project
        let project = ProjectConfig {
            repository: "https://github.com/owner/repo".to_string(),
            project_id: None,
            directory: None,
            opencode: None,
            issue_provider: "github".to_string(),
            title_pattern: "@ai.*".to_string(),
            trello_api_key: None,
            trello_token: None,
            trello_board_id: None,
        };
        let mut projects = BTreeMap::new();
        projects.insert("test-proj".to_string(), project.clone());

        let deps = WorkflowContext {
            config: GitAutomateConfig {
                projects,
                concurrency: None,
            },
            github: Some(client),
            shell: mock_shell(),
        };

        let tmp = tempfile::tempdir().unwrap();
        let yaml = "projects:\n  test-proj:\n    repository: https://github.com/owner/repo\n";
        std::fs::write(tmp.path().join("git-automate.yml"), yaml).unwrap();
        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().await;
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let workflow = Workflow::new(deps);
        let result = workflow.run_setup_check().await;

        std::env::set_current_dir(&original_dir).unwrap();

        assert!(result.is_ok());
        mock.verify().await; // create_project called exactly once
    }

    // ── run_setup_check with one failing project (test 4) ─────

    #[tokio::test]
    async fn run_setup_check_survives_project_error() {
        let mock = MockServer::start().await;

        // Project with projectId → setup_project calls ensure_status_options
        // which calls get_project_status_field. Return null → NoStatusField error.
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "node": { "field": null } }
            })))
            .mount(&mock)
            .await;

        let client = gh_client(&mock);
        let project = ProjectConfig {
            repository: "https://github.com/owner/repo".to_string(),
            project_id: Some("PID-1".to_string()),
            directory: None,
            opencode: None,
            issue_provider: "github".to_string(),
            title_pattern: "@ai.*".to_string(),
            trello_api_key: None,
            trello_token: None,
            trello_board_id: None,
        };
        let mut projects = BTreeMap::new();
        projects.insert("proj-a".to_string(), project);

        let capture = LogCapture::install();
        let deps = WorkflowContext {
            config: GitAutomateConfig {
                projects,
                concurrency: None,
            },
            github: Some(client),
            shell: mock_shell(),
        };

        let workflow = Workflow::new(deps);
        let result = workflow.run_setup_check().await;

        assert!(result.is_ok());
        let msgs = capture.messages();
        let has_error = msgs
            .iter()
            .any(|m| m.contains("[ERROR]") && m.contains("Setup check failed for proj-a"));
        assert!(has_error, "expected error log for proj-a: {:?}", msgs);
    }

    // ── run_triage_check skips project without opencode (test 5) ─

    #[tokio::test]
    async fn run_triage_check_skips_project_without_opencode() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);
        let capture = LogCapture::install();

        let project = make_project_no_opencode();
        let mut projects = BTreeMap::new();
        projects.insert("no-opencode-proj".to_string(), project);

        let deps = WorkflowContext {
            config: GitAutomateConfig {
                projects,
                concurrency: None,
            },
            github: Some(client),
            shell: mock_shell(),
        };

        let workflow = Workflow::new(deps);
        let result = workflow.run_triage_check().await;

        assert!(result.is_ok());
        let msgs = capture.messages();
        // No errors — project was skipped, not failed
        let has_error = msgs.iter().any(|m| m.contains("[ERROR]"));
        assert!(
            !has_error,
            "should not have errors for skipped project: {:?}",
            msgs
        );
    }

    // ── run_triage_check with github=None (test 6) ────────────

    #[tokio::test]
    async fn run_triage_check_with_no_github_logs_warn() {
        let deps = make_deps(None);
        let capture = LogCapture::install();

        let workflow = Workflow::new(deps);
        let result = workflow.run_triage_check().await;

        assert!(result.is_ok());
        let msgs = capture.messages();
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].contains("[WARN]"));
        assert!(msgs[0].contains("skipping triage check"));
    }

    // ── setup_project without projectId (test 7) ─────────────

    #[tokio::test]
    async fn setup_project_without_project_id_calls_create_and_write() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);

        // Mock get_owner_id
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("login"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "user": { "id": "uid" }, "organization": null }
            })))
            .mount(&mock)
            .await;

        // Mock createProjectV2 (expect 1)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "createProjectV2": { "id": "new-pid" } }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Mock status field → all options present (no add needed)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
            .mount(&mock)
            .await;

        // Mock fields → sessionId present (no add needed)
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
            .mount(&mock)
            .await;

        let project = ProjectConfig {
            repository: "https://github.com/owner/repo".to_string(),
            project_id: None,
            directory: None,
            opencode: None,
            issue_provider: "github".to_string(),
            title_pattern: "@ai.*".to_string(),
            trello_api_key: None,
            trello_token: None,
            trello_board_id: None,
        };

        let deps = WorkflowContext {
            config: GitAutomateConfig {
                projects: {
                    let mut m = BTreeMap::new();
                    m.insert("test-proj".to_string(), project.clone());
                    m
                },
                concurrency: None,
            },
            github: Some(client),
            shell: mock_shell(),
        };

        let tmp = tempfile::tempdir().unwrap();
        let yaml = "projects:\n  test-proj:\n    repository: https://github.com/owner/repo\n";
        std::fs::write(tmp.path().join("git-automate.yml"), yaml).unwrap();
        let _guard = crate::test_utils::SET_CWD_MUTEX.lock().await;
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let workflow = Workflow::new(deps);
        let result = workflow.setup_project("test-proj", &project).await;

        std::env::set_current_dir(&original_dir).unwrap();

        assert!(result.is_ok());
        mock.verify().await; // create_project called exactly once
    }

    // ── setup_project with projectId (test 8) ────────────────

    #[tokio::test]
    async fn setup_project_with_project_id_skips_create() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);

        // Mock status field → all options
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
            .mount(&mock)
            .await;

        // Mock fields → sessionId present
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
            .mount(&mock)
            .await;

        // createProjectV2 should NOT be called
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "createProjectV2": { "id": "should-not-be-called" } }
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let project = ProjectConfig {
            repository: "https://github.com/owner/repo".to_string(),
            project_id: Some("PID-123".to_string()),
            directory: None,
            opencode: None,
            issue_provider: "github".to_string(),
            title_pattern: "@ai.*".to_string(),
            trello_api_key: None,
            trello_token: None,
            trello_board_id: None,
        };

        let deps = WorkflowContext {
            config: GitAutomateConfig {
                projects: {
                    let mut m = BTreeMap::new();
                    m.insert("test-proj".to_string(), project.clone());
                    m
                },
                concurrency: None,
            },
            github: Some(client),
            shell: mock_shell(),
        };

        let workflow = Workflow::new(deps);
        let result = workflow.setup_project("test-proj", &project).await;

        assert!(result.is_ok());
    }

    // ── ensure_status_options with all options (test 9) ──────

    #[tokio::test]
    async fn ensure_status_options_all_present_no_add() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
            .mount(&mock)
            .await;

        // addProjectStatusOptions should NOT be called
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2FieldConfiguration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "updateProjectV2FieldConfiguration": { "projectV2Field": { "id": "x" } } }
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let deps = make_deps(Some(client));
        let workflow = Workflow::new(deps);
        let result = workflow.ensure_status_options("PID-123").await;

        assert!(result.is_ok());
        mock.verify().await;
    }

    // ── ensure_status_options with missing options (test 10) ─

    #[tokio::test]
    async fn ensure_status_options_missing_calls_add() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);

        // Only "Done" present → 6 missing
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("field(name:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
            .mount(&mock)
            .await;

        // addProjectStatusOptions → expect 1 call
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("updateProjectV2FieldConfiguration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "updateProjectV2FieldConfiguration": { "projectV2Field": { "id": "x" } } }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let deps = make_deps(Some(client));
        let workflow = Workflow::new(deps);
        let result = workflow.ensure_status_options("PID-123").await;

        assert!(result.is_ok());
    }

    // ── ensure_session_id_field when exists (test 11) ────────

    #[tokio::test]
    async fn ensure_session_id_field_exists_no_add() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
            .mount(&mock)
            .await;

        // createProjectV2Field should NOT be called
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2Field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "createProjectV2Field": { "projectField": { "id": "should-not" } } }
            })))
            .expect(0)
            .mount(&mock)
            .await;

        let deps = make_deps(Some(client));
        let workflow = Workflow::new(deps);
        let result = workflow.ensure_session_id_field("PID-123").await;

        assert!(result.is_ok());
        mock.verify().await;
    }

    // ── ensure_session_id_field when missing (test 12) ───────

    #[tokio::test]
    async fn ensure_session_id_field_missing_calls_add() {
        let mock = MockServer::start().await;
        let client = gh_client(&mock);

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("fields(first:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
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
            .mount(&mock)
            .await;

        // createProjectV2Field → expect 1 call
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("createProjectV2Field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": { "createProjectV2Field": { "projectField": { "id": "new-field" } } }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let deps = make_deps(Some(client));
        let workflow = Workflow::new(deps);
        let result = workflow.ensure_session_id_field("PID-123").await;

        assert!(result.is_ok());
    }

    // ── check_opencode when healthy (test 13) ────────────────

    #[tokio::test]
    async fn check_opencode_healthy_checks_agents() {
        let mock = MockServer::start().await;

        // Mock health endpoint
        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "healthy": true,
                "version": "1.0.0"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Mock agent list → all 6 required agents
        Mock::given(method("GET"))
            .and(path("/agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"name":"git-automate-triage","description":"t","mode":"subagent","builtIn":true},
                {"name":"git-automate-taskmanager","description":"t","mode":"subagent","builtIn":true},
                {"name":"git-automate-developer","description":"t","mode":"subagent","builtIn":true},
                {"name":"git-automate-reviewer","description":"t","mode":"subagent","builtIn":true},
                {"name":"git-automate-product","description":"t","mode":"subagent","builtIn":true},
                {"name":"git-automate-qa","description":"t","mode":"subagent","builtIn":true},
            ])))
            .expect(1)
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "test-pw".to_string(),
            directory: Some("/tmp/work".to_string()),
        };

        let deps = make_deps(None);
        let workflow = Workflow::new(deps);
        let result = workflow.check_opencode(&oc).await;

        assert!(result.is_ok());
        mock.verify().await;
    }

    // ── check_opencode when not healthy (test 14) ────────────

    #[tokio::test]
    async fn check_opencode_unhealthy_returns_early() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "healthy": false,
                "version": "1.0.0"
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Agent endpoint should NOT be called
        Mock::given(method("GET"))
            .and(path("/agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .expect(0)
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "test-pw".to_string(),
            directory: None,
        };

        let deps = make_deps(None);
        let workflow = Workflow::new(deps);
        let result = workflow.check_opencode(&oc).await;

        assert!(result.is_ok());
        mock.verify().await;
    }

    // ── check_opencode with missing agents (test 15) ─────────

    #[tokio::test]
    async fn check_opencode_missing_agents_warns() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "healthy": true,
                "version": "1.0.0"
            })))
            .mount(&mock)
            .await;

        // Only 2 agents (missing 4)
        Mock::given(method("GET"))
            .and(path("/agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"name":"git-automate-triage","description":"t","mode":"subagent","builtIn":true},
                {"name":"git-automate-developer","description":"t","mode":"subagent","builtIn":true},
            ])))
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "test-pw".to_string(),
            directory: None,
        };

        let capture = LogCapture::install();
        let deps = make_deps(None);

        let workflow = Workflow::new(deps);
        let result = workflow.check_opencode(&oc).await;

        assert!(result.is_ok());
        let msgs = capture.messages();
        let has_warn = msgs
            .iter()
            .any(|m| m.contains("[WARN]") && m.contains("Missing required agents"));
        assert!(
            has_warn,
            "expected 'Missing required agents' warning: {:?}",
            msgs
        );
    }

    // ── check_opencode with all agents present (test 16) ─────

    #[tokio::test]
    async fn check_opencode_all_agents_logs_info() {
        let mock = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "healthy": true,
                "version": "1.0.0"
            })))
            .mount(&mock)
            .await;

        Mock::given(method("GET"))
            .and(path("/agent"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"name":"git-automate-triage","mode":"subagent","builtIn":true},
                {"name":"git-automate-taskmanager","mode":"subagent","builtIn":true},
                {"name":"git-automate-developer","mode":"subagent","builtIn":true},
                {"name":"git-automate-reviewer","mode":"subagent","builtIn":true},
                {"name":"git-automate-product","mode":"subagent","builtIn":true},
                {"name":"git-automate-qa","mode":"subagent","builtIn":true},
            ])))
            .mount(&mock)
            .await;

        let oc = OpencodeSessionConfig {
            url: mock.uri(),
            pw: "test-pw".to_string(),
            directory: None,
        };

        let capture = LogCapture::install();
        let deps = make_deps(None);

        let workflow = Workflow::new(deps);
        let result = workflow.check_opencode(&oc).await;

        assert!(result.is_ok());
        let msgs = capture.messages();
        let has_all_present = msgs
            .iter()
            .any(|m| m.contains("[INFO]") && m.contains("All required agents present"));
        assert!(
            has_all_present,
            "expected 'All required agents present' log: {:?}",
            msgs
        );
    }

    // ── run_all with no projects returns Ok ───────────────────

    #[tokio::test]
    async fn run_all_with_no_projects_returns_ok() {
        let deps = make_deps(None);
        let workflow = Workflow::new(deps);
        let result = workflow.run_all().await;
        assert!(result.is_ok());
    }
}
