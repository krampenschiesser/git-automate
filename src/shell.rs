//! Shell execution module — mirrors the TypeScript plugin's `buildShellFn`
//! (`src/plugin.ts` lines 11-19).
//!
//! Runs commands via `sh -c <command>`, captures stdout, stderr, and the
//! exit code. stdout/stderr are decoded with `String::from_utf8_lossy`
//! so invalid UTF-8 is replaced with the replacement character rather
//! than panicking.
//!
//! Exit-code policy (matches `result.exitCode` in the TS):
//! - Normal exit → the process exit code.
//! - Killed by a signal (no exit code) → `-1`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::process::Command;

// ─── BoxFuture ─────────────────────────────────────────────────
//
// `std::future::BoxFuture` is not yet stabilised in this toolchain, so we
// provide the canonical alias (the same definition used by the `futures`
// crate).

/// A pinned, boxed, sendable future owned for `'a`.
type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ─── ShellOutput ───────────────────────────────────────────────

/// Result of a shell command execution.
#[derive(Debug)]
pub struct ShellOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

// ─── ShellFn ───────────────────────────────────────────────────

/// Type alias for the shell function used in `WorkflowDeps`.
///
/// Takes a command string, returns [`ShellOutput`] asynchronously.
/// Wrapped in `Arc` so it can be shared across async tasks.
pub type ShellFn = Arc<dyn Fn(String) -> BoxFuture<'static, ShellOutput> + Send + Sync>;

// ─── execute_shell ─────────────────────────────────────────────

/// Execute a single shell command (standalone, not wrapped in `Arc`).
///
/// Uses `sh -c <command>`.
///
/// - stdout/stderr decoded via `String::from_utf8_lossy`.
/// - exit_code = `status.code().unwrap_or(-1)`; if the process was killed
///   by a signal, `code()` returns `None` and `-1` is used.
/// - If spawning the process fails entirely, returns a `ShellOutput` with
///   empty stdout/stderr and `exit_code` `-1`.
pub async fn execute_shell(command: &str) -> ShellOutput {
    let output = Command::new("sh").arg("-c").arg(command).output().await;

    match output {
        Ok(output) => ShellOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        },
        Err(_) => ShellOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: -1,
        },
    }
}

// ─── create_shell_fn ───────────────────────────────────────────

/// Create a [`ShellFn`] that executes commands via `sh -c <command>`.
///
/// This is the Rust equivalent of `buildShellFn` in `src/plugin.ts` — it
/// returns a clonable, shareable shell function matching the `shell` field
/// of `WorkflowDeps`.
pub fn create_shell_fn() -> ShellFn {
    Arc::new(|command: String| Box::pin(async move { execute_shell(&command).await }))
}

// ─── Tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── echo tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn echo_hello_includes_newline() {
        let result = execute_shell("echo hello").await;
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn echo_n_no_newline() {
        let result = execute_shell("echo -n hello").await;
        assert_eq!(result.stdout, "hello");
        assert_eq!(result.exit_code, 0);
    }

    // ── exit code tests ─────────────────────────────────────────

    #[tokio::test]
    async fn exit_1() {
        let result = execute_shell("exit 1").await;
        assert_eq!(result.exit_code, 1);
    }

    #[tokio::test]
    async fn exit_42() {
        let result = execute_shell("exit 42").await;
        assert_eq!(result.exit_code, 42);
    }

    // ── stderr test ─────────────────────────────────────────────

    #[tokio::test]
    async fn stderr_redirect() {
        let result = execute_shell("echo err >&2").await;
        assert!(result.stderr.contains("err"));
        assert_eq!(result.exit_code, 0);
    }

    // ── nonexistent command ─────────────────────────────────────

    #[tokio::test]
    async fn nonexistent_command_fails() {
        let result = execute_shell("nonexistent_command_xyz_123").await;
        assert_ne!(result.exit_code, 0);
        assert!(!result.stderr.is_empty());
    }

    // ── UTF-8 test ──────────────────────────────────────────────

    #[tokio::test]
    async fn utf8_output() {
        let result = execute_shell("printf '%s' 'héllo'").await;
        assert_eq!(result.stdout, "héllo");
        assert_eq!(result.exit_code, 0);
    }

    // ── create_shell_fn parity tests ────────────────────────────

    #[tokio::test]
    async fn shell_fn_matches_execute_shell_echo() {
        let shell = create_shell_fn();
        let result = shell("echo hello".to_string()).await;
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.exit_code, 0);
    }

    #[tokio::test]
    async fn shell_fn_matches_execute_shell_exit_42() {
        let shell = create_shell_fn();
        let result = shell("exit 42".to_string()).await;
        assert_eq!(result.exit_code, 42);
    }

    #[tokio::test]
    async fn shell_fn_matches_execute_shell_stderr() {
        let shell = create_shell_fn();
        let result = shell("echo err >&2".to_string()).await;
        assert!(result.stderr.contains("err"));
        assert_eq!(result.exit_code, 0);
    }
}
