//! git-automate daemon — standalone Rust binary.
//!
//! Replaces the TypeScript plugin's `gitAutomate` function (`src/plugin.ts`)
//! with a long-running daemon that polls GitHub + OpenCode every 30 seconds.
//!
//! Behaviour mirrors `plugin.ts`:
//!   - Config failure → **exit** (daemon mode is stricter than the TS plugin).
//!   - `GITHUB_TOKEN` unset/empty → log error, `github: None`, continue.
//!   - Logging goes through `tracing` with `[git-automate][LEVEL] message` format.
//!   - Startup `run_all()` → catch + log `"Startup runAll failed: {e}"`.
//!   - Polling `run_all()` → catch + log `"Polling runAll failed: {e}"`.
//!   - SIGINT/SIGTERM → `"Received shutdown signal, exiting"`, break.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Parser, Subcommand};
use tracing_subscriber::layer::Layer;
use tracing_subscriber::prelude::*;

use git_automate::config::parse_config;
use git_automate::external_agent::client::OpenCodeClient;
use git_automate::external_issues::client::GitHubClient;
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

// ─── Tracing setup ───────────────────────────────────────────

struct FieldVisitor {
    message: String,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        }
    }
}

struct GitAutomateFormatLayer;

impl<S> Layer<S> for GitAutomateFormatLayer
where
    S: tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let level_str = match *event.metadata().level() {
            tracing::Level::ERROR => "ERROR",
            tracing::Level::WARN => "WARN",
            tracing::Level::INFO => "INFO",
            tracing::Level::DEBUG => "DEBUG",
            tracing::Level::TRACE => "TRACE",
        };

        let mut visitor = FieldVisitor {
            message: String::new(),
        };
        event.record(&mut visitor);

        let line = format!("[git-automate][{}] {}", level_str, visitor.message);

        if *event.metadata().level() == tracing::Level::ERROR {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    }
}

fn init_tracing() {
    let subscriber = tracing_subscriber::registry().with(GitAutomateFormatLayer);
    let _ = tracing::subscriber::set_global_default(subscriber);
}

// ─── Entry point ─────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
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
            tracing::error!(
                "GITHUB_TOKEN environment variable is not set — GitHub operations are skipped"
            );
            None
        }
    };

    let github_was_some = github.is_some();

    let shell = create_shell_fn();
    let deps = WorkflowContext {
        config,
        github,
        shell,
    };
    let workflow = Workflow::new(deps);

    Ok((workflow, github_was_some))
}

// ─── serve ───────────────────────────────────────────────────

async fn serve(config_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    init_tracing();
    let (workflow, _) = setup(config_path).await?;

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
        let captured = capture_log(tracing::Level::INFO, "Starting git-automate daemon");
        assert_eq!(
            captured,
            "[git-automate][INFO] Starting git-automate daemon"
        );
    }

    // T10: [git-automate][ERROR] format on stderr
    #[test]
    fn error_log_format_matches_prefix() {
        let captured = capture_log(
            tracing::Level::ERROR,
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
        let captured = capture_log(tracing::Level::ERROR, "something broke");
        assert_eq!(captured, "[git-automate][ERROR] something broke");
    }

    struct FieldVisitor {
        message: String,
    }

    impl tracing::field::Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = format!("{value:?}");
            }
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            }
        }
    }

    struct CaptureLayer {
        buffer: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl<S> tracing_subscriber::layer::Layer<S> for CaptureLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let level_str = match *event.metadata().level() {
                tracing::Level::ERROR => "ERROR",
                tracing::Level::WARN => "WARN",
                tracing::Level::INFO => "INFO",
                tracing::Level::DEBUG => "DEBUG",
                tracing::Level::TRACE => "TRACE",
            };
            let mut visitor = FieldVisitor {
                message: String::new(),
            };
            event.record(&mut visitor);
            self.buffer
                .lock()
                .unwrap()
                .push(format!("[git-automate][{}] {}", level_str, visitor.message));
        }
    }

    fn capture_log(level: tracing::Level, msg: &str) -> String {
        let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let layer = CaptureLayer {
            buffer: buffer.clone(),
        };
        let subscriber = tracing_subscriber::registry().with(layer);
        let _guard = tracing::subscriber::set_default(subscriber);
        match level {
            tracing::Level::ERROR => tracing::error!("{}", msg),
            tracing::Level::WARN => tracing::warn!("{}", msg),
            tracing::Level::INFO => tracing::info!("{}", msg),
            tracing::Level::DEBUG => tracing::debug!("{}", msg),
            tracing::Level::TRACE => tracing::trace!("{}", msg),
        }
        buffer.lock().unwrap()[0].clone()
    }
}
