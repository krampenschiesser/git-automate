//! Logging module — mirrors the TypeScript plugin's `log()` helper
//! (`src/plugin.ts` lines 32-45).
//!
//! Output format: `[git-automate][LEVEL] message\n`
//!
//! Routing (identical to the TS `switch`):
//! - `LogLevel::Error` → stderr (`process.stderr.write`)
//! - `LogLevel::Warn`  → stdout (`process.stdout.write`)
//! - `LogLevel::Info`  → stdout
//!
//! The trailing `\n` is supplied by `println!`/`eprintln!`, exactly as
//! the TS code does `write(\`${prefix} ${message}\\n\`)`.  New-lines
//! embedded in *message* survive verbatim (the entire message string is
//! written after the prefix, never split per-line).

use std::cell::RefCell;
use std::fmt::Write as _;

// ─── LogLevel ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

// ─── Predicates ──────────────────────────────────────────────

/// Returns `true` when *level* is routed to stderr.
pub fn is_error(level: LogLevel) -> bool {
    matches!(level, LogLevel::Error)
}

// ─── Thread-local capture sink ───────────────────────────────
//
// In production this is always `None` and `log()` falls through to
// `println!` / `eprintln!`.  Tests install a closure to capture output
// without touching real stdout/stderr.

type LogSink = Box<dyn Fn(LogLevel, &str)>;

thread_local! {
    static TEST_SINK: RefCell<Option<LogSink>> = RefCell::new(None);
}

// ─── Public API ──────────────────────────────────────────────

/// Log *message* at *level*.
///
/// Format: `[git-automate][LEVEL] message\n`
/// - `LogLevel::Error` → stderr
/// - `LogLevel::Warn`  → stdout
/// - `LogLevel::Info`  → stdout
pub fn log(level: LogLevel, message: &str) {
    // Build the full line *without* the trailing newline — println! /
    // eprintln! will add it, matching `process.stdout.write(\`${prefix} ${message}\\n\`)`.
    let mut buf =
        String::with_capacity("[git-automate][] ".len() + level.as_str().len() + message.len());
    let _ = write!(buf, "[git-automate][{}] {}", level.as_str(), message);

    TEST_SINK.with(|sink| {
        if let Some(s) = &*sink.borrow() {
            s(level, &buf);
        } else {
            match level {
                LogLevel::Error => eprintln!("{buf}"),
                _ => println!("{buf}"),
            }
        }
    });
}

/// Convenience: `log(LogLevel::Info, message)`.
pub fn info(message: &str) {
    log(LogLevel::Info, message);
}

/// Convenience: `log(LogLevel::Warn, message)`.
pub fn warn(message: &str) {
    log(LogLevel::Warn, message);
}

/// Convenience: `log(LogLevel::Error, message)`.
pub fn error(message: &str) {
    log(LogLevel::Error, message);
}

// ─── Test hooks ──────────────────────────────────────────────

pub fn set_test_sink(sink: LogSink) {
    TEST_SINK.with(|s| *s.borrow_mut() = Some(sink));
}

pub fn clear_test_sink() {
    TEST_SINK.with(|s| *s.borrow_mut() = None);
}

// ─── Tests ───────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell as StdRefCell;
    use std::rc::Rc;

    /// Holds captured stdout/stderr buffers shared with a sink closure.
    struct Capture {
        stdout: Rc<StdRefCell<String>>,
        stderr: Rc<StdRefCell<String>>,
    }

    impl Capture {
        fn new() -> (Self, impl Fn(LogLevel, &str)) {
            let stdout = Rc::new(StdRefCell::new(String::new()));
            let stderr = Rc::new(StdRefCell::new(String::new()));
            let s_out = Rc::clone(&stdout);
            let s_err = Rc::clone(&stderr);
            let sink = move |level: LogLevel, msg: &str| {
                if is_error(level) {
                    s_err.borrow_mut().push_str(msg);
                    s_err.borrow_mut().push('\n');
                } else {
                    s_out.borrow_mut().push_str(msg);
                    s_out.borrow_mut().push('\n');
                }
            };
            (Capture { stdout, stderr }, sink)
        }
    }

    /// RAII guard that installs a sink on creation and clears it on drop.
    struct SinkGuard;

    impl SinkGuard {
        fn install<F: Fn(LogLevel, &str) + 'static>(sink: F) -> Self {
            TEST_SINK.with(|s| *s.borrow_mut() = Some(Box::new(sink)));
            SinkGuard
        }
    }

    impl Drop for SinkGuard {
        fn drop(&mut self) {
            TEST_SINK.with(|s| *s.borrow_mut() = None);
        }
    }

    // ── as_str tests ──────────────────────────────────────────

    #[test]
    fn info_as_str() {
        assert_eq!(LogLevel::Info.as_str(), "INFO");
    }

    #[test]
    fn warn_as_str() {
        assert_eq!(LogLevel::Warn.as_str(), "WARN");
    }

    #[test]
    fn error_as_str() {
        assert_eq!(LogLevel::Error.as_str(), "ERROR");
    }

    // ── is_error tests ────────────────────────────────────────

    #[test]
    fn is_error_returns_false_for_info() {
        assert!(!is_error(LogLevel::Info));
    }

    #[test]
    fn is_error_returns_true_for_error() {
        assert!(is_error(LogLevel::Error));
    }

    #[test]
    fn is_error_returns_false_for_warn() {
        assert!(!is_error(LogLevel::Warn));
    }

    // ── log() routing tests ───────────────────────────────────

    #[test]
    fn log_info_goes_to_stdout() {
        let (cap, sink) = Capture::new();
        let _guard = SinkGuard::install(sink);

        log(LogLevel::Info, "hello");

        assert_eq!(cap.stdout.borrow().as_str(), "[git-automate][INFO] hello\n");
        assert!(cap.stderr.borrow().is_empty());
    }

    #[test]
    fn log_warn_goes_to_stdout() {
        let (cap, sink) = Capture::new();
        let _guard = SinkGuard::install(sink);

        log(LogLevel::Warn, "careful");

        assert_eq!(
            cap.stdout.borrow().as_str(),
            "[git-automate][WARN] careful\n"
        );
        assert!(cap.stderr.borrow().is_empty());
    }

    #[test]
    fn log_error_goes_to_stderr() {
        let (cap, sink) = Capture::new();
        let _guard = SinkGuard::install(sink);

        log(LogLevel::Error, "bad");

        assert_eq!(cap.stderr.borrow().as_str(), "[git-automate][ERROR] bad\n");
        assert!(cap.stdout.borrow().is_empty());
    }

    // ── convenience functions ─────────────────────────────────

    #[test]
    fn info_convenience() {
        let (cap, sink) = Capture::new();
        let _guard = SinkGuard::install(sink);

        info("hello");

        assert_eq!(cap.stdout.borrow().as_str(), "[git-automate][INFO] hello\n");
        assert!(cap.stderr.borrow().is_empty());
    }

    #[test]
    fn warn_convenience() {
        let (cap, sink) = Capture::new();
        let _guard = SinkGuard::install(sink);

        warn("careful");

        assert_eq!(
            cap.stdout.borrow().as_str(),
            "[git-automate][WARN] careful\n"
        );
        assert!(cap.stderr.borrow().is_empty());
    }

    #[test]
    fn error_convenience() {
        let (cap, sink) = Capture::new();
        let _guard = SinkGuard::install(sink);

        error("bad");

        assert_eq!(cap.stderr.borrow().as_str(), "[git-automate][ERROR] bad\n");
        assert!(cap.stdout.borrow().is_empty());
    }

    // ── multi-line message ────────────────────────────────────

    #[test]
    fn log_multiline_message_preserves_internal_newlines() {
        // TS does: write(`${prefix} ${message}\n`) — the entire message
        // (including internal newlines) follows the prefix.  Only a
        // single \n is appended at the end.
        let (cap, sink) = Capture::new();
        let _guard = SinkGuard::install(sink);

        log(LogLevel::Info, "line1\nline2");

        assert_eq!(
            cap.stdout.borrow().as_str(),
            "[git-automate][INFO] line1\nline2\n"
        );
    }

    // ── empty message ─────────────────────────────────────────

    #[test]
    fn log_empty_message() {
        let (cap, sink) = Capture::new();
        let _guard = SinkGuard::install(sink);

        log(LogLevel::Info, "");

        assert_eq!(cap.stdout.borrow().as_str(), "[git-automate][INFO] \n");
    }
}
