//! git-automate daemon — standalone Rust binary.
//!
//! A long-running daemon that polls GitHub + OpenCode every 30 seconds.
//!
//! Behaviour:
//!   - Config failure → **exit** (daemon mode is stricter than the TS plugin).
//!   - `GITHUB_TOKEN` unset/empty → **exit** (fail fast).
//!   - Logging goes through `tracing` with `[git-automate][LEVEL] message` format.
//!   - Startup `run_all()` → catch + log `"Startup runAll failed: {e}"`.
//!   - Polling `run_all()` → catch + log `"Polling runAll failed: {e}"`.
//!   - SIGINT/SIGTERM → `"Received shutdown signal, exiting"`, break.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};

use git_automate::config::parse_config;
use git_automate::external_agent::opencode::OpenCodeClient;
use git_automate::external_issues::github::client::GitHubClient;
use git_automate::shell::create_shell_fn;
use git_automate::workflow::Workflow;
use git_automate::workflow::helpers::WorkflowContext;

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
        #[arg(long, default_value = git_automate::config::DEFAULT_CONFIG_FILE)]
        config: PathBuf,
    },
    /// Check OpenCode server health
    Health {
        #[arg(long)]
        url: String,
        #[arg(long)]
        pw: String,
    },
    /// Check project setup, OpenCode health, and copy missing agents
    Doctor {
        /// Path to the git-automate.yml config file
        #[arg(long, default_value = git_automate::config::DEFAULT_CONFIG_FILE)]
        config: PathBuf,
    },
}

// ─── Entry point ─────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    // use that subscriber to process traces emitted after this point
    tracing::subscriber::set_global_default(subscriber)?;
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { config } => serve(&config).await,
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

    let token = std::env::var("GITHUB_TOKEN")
        .map_err(|_| "GITHUB_TOKEN environment variable is not set — cannot start daemon")?;
    if token.is_empty() {
        return Err("GITHUB_TOKEN environment variable is empty — cannot start daemon".into());
    }
    let github = GitHubClient::new(token)?;

    let shell = create_shell_fn();
    let deps = WorkflowContext {
        config,
        github: Some(github),
        shell,
    };
    let workflow = Workflow::new(deps);

    Ok(workflow)
}

// ─── doctor_setup ────────────────────────────────────────────

/// Like [`setup`], but treats `GITHUB_TOKEN` as optional — warns and
/// continues with `github: None` when the token is unset or empty.
async fn doctor_setup(config_path: &Path) -> Result<Workflow, Box<dyn std::error::Error>> {
    let config = parse_config(config_path)?;

    let github = match std::env::var("GITHUB_TOKEN") {
        Ok(token) if !token.is_empty() => Some(GitHubClient::new(token)?),
        _ => {
            tracing::warn!("GITHUB_TOKEN not set or empty — GitHub checks will be skipped");
            None
        }
    };

    let shell = create_shell_fn();
    let deps = WorkflowContext {
        config,
        github,
        shell,
    };
    let workflow = Workflow::new(deps);

    Ok(workflow)
}

// ─── serve ───────────────────────────────────────────────────

async fn serve(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let workflow = setup(config_path).await?;

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

// ─── doctor ────────────────────────────────────────────────

/// Run a one-shot doctor check (no polling loop).
async fn doctor(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let workflow = doctor_setup(config_path).await?;

    tracing::info!("Running doctor checks");
    if let Err(e) = workflow.run_doctor_check().await {
        tracing::error!("Doctor check failed: {}", e);
    }

    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::LazyLock;
    use tokio::sync::Mutex;

    use serde_json::json;
    use wiremock::matchers::{method, path};
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
        std::fs::write(
            &config_path,
            "projects:\n  test:\n    repository: owner/repo\n",
        )
        .expect("write config");

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
        std::fs::write(
            &config_path,
            "projects:\n  test:\n    repository: owner/repo\n",
        )
        .expect("write config");

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

    // T-n: doctor_setup without GITHUB_TOKEN → warns, github=None, succeeds
    #[tokio::test]
    async fn doctor_setup_no_github_token_warns() {
        let _guard = ENV_LOCK.lock().await;
        let saved = std::env::var("GITHUB_TOKEN").ok();
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("git-automate.yml");
        std::fs::write(
            &config_path,
            "projects:\n  test:\n    repository: owner/repo\n",
        )
        .expect("write config");

        let result = doctor_setup(&config_path).await;
        assert!(
            result.is_ok(),
            "doctor_setup should succeed without GITHUB_TOKEN"
        );

        if let Some(val) = saved {
            unsafe {
                std::env::set_var("GITHUB_TOKEN", val);
            }
        }
    }

    // T-n: doctor_setup with GITHUB_TOKEN → succeeds, github=Some
    #[tokio::test]
    async fn doctor_setup_with_github_token_succeeds() {
        let _guard = ENV_LOCK.lock().await;
        let saved = std::env::var("GITHUB_TOKEN").ok();
        unsafe {
            std::env::set_var("GITHUB_TOKEN", "ghp_testtoken123456789");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("git-automate.yml");
        std::fs::write(
            &config_path,
            "projects:\n  test:\n    repository: owner/repo\n",
        )
        .expect("write config");

        let result = doctor_setup(&config_path).await;
        assert!(
            result.is_ok(),
            "doctor_setup should succeed: {:?}",
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
}
