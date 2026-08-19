//! Integration tests for SIGTERM signal handling in the daemon loop.
//!
//! These tests verify that the `serve` command exits cleanly when receiving
//! SIGTERM, logging the expected shutdown message, without hanging or panicking.

#[cfg(unix)]
mod sigterm_tests {
    use std::process::Stdio;
    use std::time::Duration;

    use tokio::io::AsyncReadExt;
    use tokio::process::Command;
    use tokio::time::timeout;

    /// T-n: SIGTERM causes daemon to exit cleanly with shutdown message
    #[tokio::test]
    async fn sigterm_exits_cleanly_with_shutdown_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("git-automate.yml");
        std::fs::write(
            &config_path,
            "git:\n  repository: https://github.com/owner/repo\n  directory: /tmp\n",
        )
        .expect("write config");

        let binary = env!("CARGO_BIN_EXE_git-automate");
        let mut child = Command::new(binary)
            .arg("serve")
            .arg("--config")
            .arg(&config_path)
            .env("GITHUB_TOKEN", "ghp_testtoken123456789")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn daemon");

        // Give the daemon time to start and enter the polling loop
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Send SIGTERM
        let pid = child.id().expect("child process should have a PID");
        std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .spawn()
            .expect("failed to send SIGTERM")
            .wait()
            .expect("kill command failed");

        // Wait for the daemon to exit (must not hang)
        let wait_result = timeout(Duration::from_secs(5), child.wait()).await;
        assert!(
            wait_result.is_ok(),
            "daemon did not exit within 5s after SIGTERM — possible hang"
        );
        let status = wait_result.unwrap().expect("daemon exited with error");
        assert!(
            status.success(),
            "daemon should exit with code 0, got: {}",
            status
        );

        let mut stdout_buf = Vec::new();
        child
            .stdout
            .take()
            .expect("stdout not piped")
            .read_to_end(&mut stdout_buf)
            .await
            .expect("failed to read stdout");
        let stdout = String::from_utf8_lossy(&stdout_buf);
        assert!(
            stdout.contains("Received shutdown signal, exiting"),
            "stdout should contain shutdown message, got:\n{}",
            stdout
        );
    }

    /// T-n: SIGTERM does not cause the daemon to hang or panic
    #[tokio::test]
    async fn sigterm_no_hang_no_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("git-automate.yml");
        std::fs::write(
            &config_path,
            "git:\n  repository: https://github.com/owner/repo\n  directory: /tmp\n",
        )
        .expect("write config");

        let binary = env!("CARGO_BIN_EXE_git-automate");
        let child = Command::new(binary)
            .arg("serve")
            .arg("--config")
            .arg(&config_path)
            .env("GITHUB_TOKEN", "ghp_testtoken123456789")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn daemon");

        // Give the daemon time to start
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Send SIGTERM
        let pid = child.id().expect("child process should have a PID");
        std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .spawn()
            .expect("failed to send SIGTERM")
            .wait()
            .expect("kill command failed");

        // Verify exit within 5s (no hang) and no panic (clean exit)
        let result = timeout(Duration::from_secs(5), child.wait_with_output()).await;
        assert!(
            result.is_ok(),
            "daemon did not exit within 5s after SIGTERM — hang detected"
        );
        let output = result.unwrap().expect("daemon exited with error");
        assert!(
            output.status.success(),
            "daemon should exit cleanly (no panic), got status: {}",
            output.status
        );
    }
}
