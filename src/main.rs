//! git-automate daemon — standalone Rust binary.
//!
//! Replaces the TypeScript plugin's `gitAutomate` function (`src/plugin.ts`)
//! with a long-running daemon that polls GitHub + OpenCode every 30 seconds.
//!
//! Behaviour mirrors `plugin.ts`:
//!   - Config failure → **exit** (daemon mode is stricter than the TS plugin).
//!   - `GITHUB_TOKEN` unset/empty → log error, `github: None`, continue.
//!   - `on_log` callback routes workflow messages through `log()`.
//!   - Startup `run_all()` → catch + log `"Startup runAll failed: {e}"`.
//!   - Polling `run_all()` → catch + log `"Polling runAll failed: {e}"`.
//!   - SIGINT/SIGTERM → `"Received shutdown signal, exiting"`, break.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

use git_automate::config::parse_config;
use git_automate::github::client::GitHubClient;
use git_automate::log::{LogLevel, log};
use git_automate::opencode::client::OpenCodeClient;
use git_automate::shell::create_shell_fn;
use git_automate::workflow::Workflow;
use git_automate::workflow::helpers::{LogFn, WorkflowDeps};

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
        #[arg(long, default_value = "git-automate.yml")]
        config: PathBuf,
    },
    /// Check OpenCode server health
    Health {
        #[arg(long)]
        url: String,
        #[arg(long)]
        pw: String,
    },
}

// ─── Entry point ─────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Serve { config } => serve(&config).await,
        Commands::Health { url, pw } => check_health(&url, &pw).await,
    }
}

// ─── setup — extracted for testability ───────────────────────

/// Load config + create clients, returning the workflow and whether
/// a `GitHubClient` was created.
///
/// Extracted from `serve` so tests can verify config loading and GitHub
/// client creation **without** entering the infinite polling loop.
async fn setup(config_path: &Path) -> Result<(Workflow, bool), Box<dyn std::error::Error>> {
    let config = parse_config(config_path)?;

    let github = match std::env::var("GITHUB_TOKEN") {
        Ok(token) if !token.is_empty() => Some(GitHubClient::new(token)?),
        _ => {
            log(
                LogLevel::Error,
                "GITHUB_TOKEN environment variable is not set — GitHub operations are skipped",
            );
            None
        }
    };

    let github_was_some = github.is_some();

    let shell = create_shell_fn();
    let on_log: LogFn = Arc::new(|level: LogLevel, msg: &str| {
        log(level, msg);
    });
    let deps = WorkflowDeps {
        config,
        github,
        shell,
        on_log: Some(on_log),
    };
    let workflow = Workflow::new(deps);

    Ok((workflow, github_was_some))
}

// ─── serve ───────────────────────────────────────────────────

async fn serve(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let (workflow, _) = setup(config_path).await?;

    log(LogLevel::Info, "Starting git-automate daemon");
    if let Err(e) = workflow.run_all().await {
        log(LogLevel::Error, &format!("Startup runAll failed: {}", e));
    }

    let mut interval = tokio::time::interval(Duration::from_secs(30));
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            _ = interval.tick() => {
                if let Err(e) = workflow.run_all().await {
                    log(
                        LogLevel::Error,
                        &format!("Polling runAll failed: {}", e),
                    );
                }
            }
            _ = &mut ctrl_c => {
                log(LogLevel::Info, "Received shutdown signal, exiting");
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
        println!("healthy");
        Ok(())
    } else {
        eprintln!("not healthy");
        std::process::exit(1);
    }
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::Mutex;

    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // T1: nonexistent config → error (does not panic)
    #[tokio::test]
    async fn serve_nonexistent_config_returns_error() {
        let result = serve(Path::new("/nonexistent/config.yml")).await;
        assert!(result.is_err());
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("not found"),
            "error should mention 'not found': {msg}"
        );
    }

    // T2: valid config + no GITHUB_TOKEN → github None
    #[tokio::test]
    async fn setup_no_github_token_creates_workflow_without_github() {
        let _guard = ENV_LOCK.lock().unwrap();
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
        assert!(result.is_ok(), "setup should succeed: {:?}", result.err());

        let (_workflow, github_was_some) = result.unwrap();
        assert!(
            !github_was_some,
            "github should be None when token is unset"
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
        let _guard = ENV_LOCK.lock().unwrap();
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

        let (_workflow, github_was_some) = result.unwrap();
        assert!(github_was_some, "github should be Some when token is set");

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

    // T9: [git-automate][INFO] format on stdout
    #[test]
    fn info_log_format_matches_prefix() {
        let captured = capture_log(LogLevel::Info, "Starting git-automate daemon");
        assert_eq!(
            captured,
            "[git-automate][INFO] Starting git-automate daemon\n"
        );
    }

    // T10: [git-automate][ERROR] format on stderr
    #[test]
    fn error_log_format_matches_prefix() {
        let captured = capture_log(
            LogLevel::Error,
            "GITHUB_TOKEN environment variable is not set — GitHub operations are skipped",
        );
        assert!(
            captured.starts_with("[git-automate][ERROR] "),
            "error log should start with '[git-automate][ERROR] ': {captured}"
        );
        assert!(
            captured.contains("GITHUB_TOKEN"),
            "error log should contain GITHUB_TOKEN: {captured}"
        );
    }

    // T10 (cont): error_log routes to stderr (via the sink)
    #[test]
    fn error_log_routes_to_stderr() {
        let captured = capture_log(LogLevel::Error, "something broke");
        assert_eq!(captured.trim_end(), "[git-automate][ERROR] something broke");
    }

    fn capture_log(level: LogLevel, msg: &str) -> String {
        let buf: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let buf_clone = Rc::clone(&buf);
        let sink: Box<dyn Fn(LogLevel, &str) + 'static> = Box::new(move |_lvl, m| {
            buf_clone.borrow_mut().push_str(m);
            buf_clone.borrow_mut().push('\n');
        });
        git_automate::log::set_test_sink(sink);
        log(level, msg);
        git_automate::log::clear_test_sink();
        buf.borrow().clone()
    }
}
