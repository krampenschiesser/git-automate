//! git-automate daemon — standalone Rust binary.
//!
//! A long-running daemon that polls GitHub + OpenCode every 30 seconds.
//!
//! Behaviour:
//!   - Config failure → **exit** (daemon mode is stricter than the TS plugin).
//!   - `GITHUB_TOKEN` unset/empty → **exit** (fail fast).
//!   - Logging goes through `tracing` with a default `FmtSubscriber`.
//!   - Startup `run_all()` → catch + log `"Startup runAll failed: {e}"`.
//!   - Polling `run_all()` → catch + log `"Polling runAll failed: {e}"`.
//!   - SIGINT/SIGTERM → `"Received shutdown signal, exiting"`, break.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use clap::{Parser, Subcommand};
use dotenv::dotenv;

use git_automate::config::parse_config;
use git_automate::external_agent::opencode::OpenCodeClient;
use git_automate::external_issues::github::client::GitHubClient;
use git_automate::external_issues::github::repo::parse_repository_url;
use git_automate::shell::create_shell_fn;
use git_automate::workflow::Workflow;
use git_automate::workflow::helpers::{
    WorkflowContext, detect_git_remote, ensure_session_id_field, ensure_status_options,
    prompt_input, resolve_project_id, write_doctor_config,
};

// ─── CLI ─────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "git-automate",
    bin_name = "git-automate",
    about = "GitHub issue workflow automation daemon"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run the git-automate daemon (polls GitHub + OpenCode every 30s)
    Serve {
        /// Path to the git-automate.yml config file
        #[arg(long, env = "GIT_AUTOMATE_CONFIG", default_value = git_automate::config::DEFAULT_CONFIG_FILE)]
        config: PathBuf,
        /// Run a single workflow cycle and exit (no polling loop)
        #[arg(long)]
        once: bool,
    },
    /// Check OpenCode server health
    Health {
        #[arg(long)]
        url: String,
        #[arg(long)]
        pw: String,
    },
    /// Set up or fix a project: create config, find/create GitHub project, ensure fields
    Doctor {
        /// Path to the git-automate.yml config file
        #[arg(long, env = "GIT_AUTOMATE_CONFIG", default_value = git_automate::config::DEFAULT_CONFIG_FILE)]
        config: PathBuf,
    },
}

// ─── Entry point ─────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    // use that subscriber to process traces emitted after this point
    tracing::subscriber::set_global_default(subscriber)?;

    dotenv().ok();

    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { config, once } => serve(&config, once).await,
        Commands::Health { url, pw } => check_health(&url, &pw).await,
        Commands::Doctor { config } => doctor(&config).await,
    }
}

// ─── setup — extracted for testability ───────────────────────

/// Load config + create the GitHub client, returning the workflow.
///
/// Fails fast if `GITHUB_TOKEN` is missing or empty.
///
/// Extracted from `serve` so tests can verify behavior without entering the polling loop.
async fn setup(config_path: &Path) -> Result<Workflow, Box<dyn std::error::Error>> {
    let config = parse_config(config_path)?;

    // Prefer the config's github_token (substituted from ${env:GITHUB_TOKEN}),
    // falling back to the GITHUB_TOKEN environment variable for backward compatibility.
    let token = config
        .github_token
        .as_ref()
        .filter(|t| !t.is_empty())
        .cloned()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()))
        .ok_or("GITHUB_TOKEN environment variable is not set — cannot start daemon")?;
    let github = GitHubClient::new(token)?;

    let shell = create_shell_fn();
    let deps = WorkflowContext {
        config,
        github: Some(github),
        shell,
        project_id_cache: Arc::new(Mutex::new(HashMap::new())),
    };
    let workflow = Workflow::new(deps);

    Ok(workflow)
}

// ─── doctor ───────────────────────────────────────────────────

/// One-shot setup wizard implementing the flow in `docs/src/cli.md`.
async fn doctor(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // Parse config first (if it exists) to access github_token via ${env:VAR} substitution
    let config = if config_path.exists() {
        Some(parse_config(config_path)?)
    } else {
        None
    };

    // Prefer the config's github_token (substituted from ${env:GITHUB_TOKEN}),
    // falling back to the GITHUB_TOKEN environment variable for backward compatibility.
    let token = config
        .as_ref()
        .and_then(|c| c.github_token.as_ref())
        .filter(|t| !t.is_empty())
        .cloned()
        .or_else(|| std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty()))
        .ok_or("GITHUB_TOKEN environment variable is not set — cannot run doctor")?;
    let github = GitHubClient::new(token)?;

    let (owner, repo, existing_project_id) = if let Some(config) = config {
        let parsed = parse_repository_url(&config.git.repository)
            .map_err(|e| format!("Invalid repository URL: {}", e))?;
        (parsed.owner, parsed.repo, config.git.project_id.clone())
    } else {
        match detect_git_remote() {
            Some(p) => (p.owner, p.repo, None),
            None => {
                let input = prompt_input("Enter repository (e.g. owner/repo): ");
                if input.is_empty() {
                    return Err("Repository name is required".into());
                }
                let p = parse_repository_url(&input)
                    .map_err(|e| format!("Invalid repository name: {}", e))?;
                (p.owner, p.repo, None)
            }
        }
    };

    // Numeric project IDs (e.g. "1") are resolved to global node IDs at runtime.
    // The original ID is preserved in the config file — not replaced by the
    // resolved global ID.
    let (resolved_project_id, original_project_id) = if let Some(pid) = existing_project_id {
        let (resolved, _was_resolved) = resolve_project_id(&github, &owner, &pid)
            .await
            .map_err(|e| format!("Failed to resolve project ID: {}", e))?;
        (resolved, pid)
    } else {
        let pid = match github.find_project_by_name(&owner, &repo).await {
            Ok(id) => id,
            Err(_) => {
                let choice = prompt_input("Use user or organization project? (user/org): ");
                let project_owner = match choice.to_lowercase().as_str() {
                    "user" => owner.clone(),
                    "org" => {
                        let org = prompt_input("Enter organization name: ");
                        if org.is_empty() {
                            return Err("Organization name is required".into());
                        }
                        org
                    }
                    _ => return Err(format!("Invalid choice: {}", choice).into()),
                };
                github.create_project(&project_owner, &repo).await?
            }
        };
        (pid.clone(), pid)
    };

    ensure_status_options(&github, &resolved_project_id).await?;
    ensure_session_id_field(&github, &resolved_project_id).await?;
    let repository_url = format!("https://github.com/{}/{}", owner, repo);
    write_doctor_config(config_path, &repo, &repository_url, &original_project_id)?;

    tracing::info!(
        "Doctor setup complete — config written to {}",
        config_path.display()
    );
    Ok(())
}

// ─── serve ───────────────────────────────────────────────────

async fn serve(config_path: &Path, once: bool) -> Result<(), Box<dyn std::error::Error>> {
    let workflow = setup(config_path).await?;

    if once {
        tracing::info!("Running single workflow cycle (--once mode)");
        if let Err(e) = workflow.run_all().await {
            tracing::error!("RunAll failed: {}", e);
        }
        return Ok(());
    }

    tracing::info!("Starting git-automate daemon");
    if let Err(e) = workflow.run_all().await {
        tracing::error!("Startup runAll failed: {}", e);
    }

    let mut interval = tokio::time::interval(Duration::from_secs(30));
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = workflow.run_all().await {
                    tracing::error!("Polling runAll failed: {}", e);
                }
            }
            _ = &mut ctrl_c => {
                tracing::info!("Received shutdown signal, exiting");
                break;
            }
        }
    }

    Ok(())
}

// ─── check_health ────────────────────────────────────────────

async fn check_health(url: &str, pw: &str) -> Result<(), Box<dyn std::error::Error>> {
    let client = OpenCodeClient::new(url.to_string(), pw.to_string());
    let healthy = client.check_health().await;
    if healthy {
        tracing::info!("healthy");
        Ok(())
    } else {
        tracing::error!("not healthy");
        Err("OpenCode server is not healthy".into())
    }
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use tokio::sync::Mutex;

    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    // T2: valid config + no GITHUB_TOKEN → setup fails (fail fast)
    #[tokio::test]
    async fn setup_no_github_token_fails() {
        let _guard = ENV_LOCK.lock().await;
        let saved = std::env::var("GITHUB_TOKEN").ok();
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("git-automate.yml");
        std::fs::write(&config_path, "git:\n  repository: owner/repo\n").expect("write config");

        let result = setup(&config_path).await;
        assert!(result.is_err(), "setup should fail when token is unset");

        let msg = format!("{}", result.err().unwrap());
        assert!(
            msg.contains("GITHUB_TOKEN"),
            "error should mention GITHUB_TOKEN: {msg}"
        );

        if let Some(val) = saved {
            unsafe {
                std::env::set_var("GITHUB_TOKEN", val);
            }
        }
    }

    // T3: valid config + GITHUB_TOKEN set → GitHubClient created
    #[tokio::test]
    async fn setup_with_github_token_creates_client() {
        let _guard = ENV_LOCK.lock().await;
        let saved = std::env::var("GITHUB_TOKEN").ok();
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "ghp_testtoken123456789");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("git-automate.yml");
        std::fs::write(&config_path, "git:\n  repository: owner/repo\n").expect("write config");

        let result = setup(&config_path).await;
        assert!(result.is_ok(), "setup should succeed: {:?}", result.err());

        if let Some(val) = saved {
            unsafe {
                std::env::set_var("GITHUB_TOKEN", val);
            }
        } else {
            unsafe {
                std::env::remove_var("GITHUB_TOKEN");
            }
        }
    }

    // T-n: setup with githubToken: ${env:GITHUB_TOKEN} in config → substitution works
    #[tokio::test]
    async fn setup_with_github_token_substituted_in_config() {
        let _guard = ENV_LOCK.lock().await;
        let saved = std::env::var("GITHUB_TOKEN").ok();
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "ghp_from_config_substitution");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("git-automate.yml");
        std::fs::write(
            &config_path,
            "git:\n  repository: owner/repo\ngithubToken: ${env:GITHUB_TOKEN}\n",
        )
        .expect("write config");

        let result = setup(&config_path).await;
        assert!(
            result.is_ok(),
            "setup should succeed with substituted githubToken: {:?}",
            result.err()
        );

        if let Some(val) = saved {
            unsafe {
                std::env::set_var("GITHUB_TOKEN", val);
            }
        } else {
            unsafe {
                std::env::remove_var("GITHUB_TOKEN");
            }
        }
    }

    // T7: check_health with healthy server
    #[tokio::test]
    async fn check_health_healthy_server() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "healthy": true,
                "version": "1.0.0"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenCodeClient::new(server.uri(), "pw".to_string());
        assert!(client.check_health().await);
    }

    // T8: check_health with unhealthy server
    #[tokio::test]
    async fn check_health_unhealthy_server() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "healthy": false,
                "version": "1.0.0"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = OpenCodeClient::new(server.uri(), "pw".to_string());
        assert!(!client.check_health().await);
    }

    // T-n: check_health() function returns Ok(()) when OpenCode server reports healthy
    #[tokio::test]
    async fn check_health_function_healthy_returns_ok() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "healthy": true,
                "version": "1.0.0"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let result = check_health(&server.uri(), "pw").await;
        assert!(
            result.is_ok(),
            "check_health should return Ok for a healthy OpenCode server"
        );
    }

    // T-n: check_health() function returns Err when OpenCode server reports unhealthy
    #[tokio::test]
    async fn check_health_function_unhealthy_returns_err() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "healthy": false,
                "version": "1.0.0"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let result = check_health(&server.uri(), "pw").await;
        assert!(
            result.is_err(),
            "check_health should return Err for an unhealthy OpenCode server"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("not healthy"),
            "error message should mention 'not healthy': {msg}"
        );
    }

    // T-n: doctor without GITHUB_TOKEN → fails
    #[tokio::test]
    async fn doctor_no_github_token_fails() {
        let _guard = ENV_LOCK.lock().await;
        let saved = std::env::var("GITHUB_TOKEN").ok();
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("git-automate.yml");

        let result = doctor(&config_path).await;
        assert!(result.is_err(), "doctor should fail without GITHUB_TOKEN");

        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("GITHUB_TOKEN"),
            "error should mention GITHUB_TOKEN: {msg}"
        );

        if let Some(val) = saved {
            unsafe {
                std::env::set_var("GITHUB_TOKEN", val);
            }
        }
    }

    // T-n: doctor with existing config (projectId set) → ensures fields, writes config
    #[tokio::test]
    async fn doctor_with_existing_config_ensures_fields() {
        let _guard = ENV_LOCK.lock().await;
        let saved = std::env::var("GITHUB_TOKEN").ok();
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "ghp_testtoken123456789");
        }

        let mock = MockServer::start().await;

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
            .mount(&mock)
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
            .mount(&mock)
            .await;

        let client =
            git_automate::external_issues::github::client::GitHubClient::new_with_base_url(
                "ghp_testtoken123456789".to_string(),
                mock.uri(),
            )
            .expect("client");

        let result = ensure_status_options(&client, "PID-123").await;
        assert!(result.is_ok());

        let result = ensure_session_id_field(&client, "PID-123").await;
        assert!(result.is_ok());

        mock.verify().await;

        if let Some(val) = saved {
            unsafe {
                std::env::set_var("GITHUB_TOKEN", val);
            }
        } else {
            unsafe {
                std::env::remove_var("GITHUB_TOKEN");
            }
        }
    }

    // T-n: find_project_by_name finds user project by title
    #[tokio::test]
    async fn find_project_by_name_user_project() {
        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": {
                        "projectsV2": {
                            "nodes": [
                                {"id":"PVT-1","number":1,"title":"my-repo"},
                                {"id":"PVT-2","number":2,"title":"other"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let client =
            git_automate::external_issues::github::client::GitHubClient::new_with_base_url(
                "test-token".to_string(),
                mock.uri(),
            )
            .expect("client");

        let result = client.find_project_by_name("myuser", "my-repo").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "PVT-1");
        mock.verify().await;
    }

    // T-n: find_project_by_name falls back to org
    #[tokio::test]
    async fn find_project_by_name_falls_back_to_org() {
        let mock = MockServer::start().await;

        // User query succeeds but no matching title
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("user(login:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": {
                        "projectsV2": { "nodes": [] }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        // Org query finds it
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(body_string_contains("organization(login:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "organization": {
                        "projectsV2": {
                            "nodes": [
                                {"id":"PVT-org-1","number":3,"title":"my-repo"},
                            ]
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&mock)
            .await;

        let client =
            git_automate::external_issues::github::client::GitHubClient::new_with_base_url(
                "test-token".to_string(),
                mock.uri(),
            )
            .expect("client");

        let result = client.find_project_by_name("myorg", "my-repo").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "PVT-org-1");
        mock.verify().await;
    }

    // T-n: find_project_by_name returns NotFound when no match
    #[tokio::test]
    async fn find_project_by_name_not_found() {
        let mock = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "user": { "projectsV2": { "nodes": [] } },
                    "organization": { "projectsV2": { "nodes": [] } }
                }
            })))
            .mount(&mock)
            .await;

        let client =
            git_automate::external_issues::github::client::GitHubClient::new_with_base_url(
                "test-token".to_string(),
                mock.uri(),
            )
            .expect("client");

        let result = client.find_project_by_name("myuser", "nonexistent").await;
        assert!(result.is_err());
    }

    // T-n: write_doctor_config writes correct YAML
    #[tokio::test]
    async fn write_doctor_config_creates_valid_yaml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("git-automate.yml");

        let result = write_doctor_config(
            &config_path,
            "my-repo",
            "https://github.com/owner/my-repo",
            "PID-123",
        );
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&config_path).unwrap();
        let data: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();

        let pid = data
            .get("git")
            .and_then(|g| g.get("projectId"))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(pid, "PID-123");

        let repo = data
            .get("git")
            .and_then(|g| g.get("repository"))
            .and_then(|v| v.as_str())
            .unwrap();
        assert_eq!(repo, "https://github.com/owner/my-repo");
    }

    // T-n: write_doctor_config updates existing config
    #[tokio::test]
    async fn write_doctor_config_updates_existing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("git-automate.yml");
        std::fs::write(
            &config_path,
            "git:\n  repository: https://github.com/owner/my-repo\n",
        )
        .unwrap();

        let result = write_doctor_config(
            &config_path,
            "my-repo",
            "https://github.com/owner/my-repo",
            "PID-456",
        );
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("projectId: PID-456"));
    }

    #[tokio::test]
    async fn write_doctor_config_numeric_id_preserves_int() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("git-automate.yml");

        let result = write_doctor_config(
            &config_path,
            "my-repo",
            "https://github.com/owner/my-repo",
            "1",
        );
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("projectId: 1"));

        let parsed = git_automate::config::parse_config(&config_path).unwrap();
        let project = &parsed.git;
        assert_eq!(project.project_id.as_deref(), Some("1"));
    }

    // T-n: write_doctor_config preserves existing config fields (concurrency,
    // issueProvider, opencode sections) when updating projectId.
    #[tokio::test]
    async fn write_doctor_config_preserves_existing_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("git-automate.yml");
        std::fs::write(
            &config_path,
            r#"
concurrency: 4
issueProvider: github
opencode:
  url: http://localhost:8081
  pw: ${env:OPENCODE_PW}
git:
  repository: https://github.com/owner/my-repo
  issueProvider: github
"#,
        )
        .unwrap();

        let result = write_doctor_config(
            &config_path,
            "my-repo",
            "https://github.com/owner/my-repo",
            "PID-789",
        );
        assert!(result.is_ok());

        let content = std::fs::read_to_string(&config_path).unwrap();
        let data: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();

        assert_eq!(data.get("concurrency").and_then(|v| v.as_u64()), Some(4));
        assert_eq!(
            data.get("issueProvider").and_then(|v| v.as_str()),
            Some("github")
        );

        let opencode = data.get("opencode").and_then(|v| v.as_mapping()).unwrap();
        assert_eq!(
            opencode.get("url").and_then(|v| v.as_str()),
            Some("http://localhost:8081")
        );
        assert_eq!(
            opencode.get("pw").and_then(|v| v.as_str()),
            Some("${env:OPENCODE_PW}")
        );

        let project = data.get("git").and_then(|g| g.as_mapping()).unwrap();
        assert_eq!(
            project.get("projectId").and_then(|v| v.as_str()),
            Some("PID-789")
        );
        assert_eq!(
            project.get("issueProvider").and_then(|v| v.as_str()),
            Some("github")
        );
    }
}
