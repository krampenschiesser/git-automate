//! git-automate: A standalone Rust daemon that automates GitHub issue workflows
//! via OpenCode agent sessions.

pub mod config;
pub mod issues;
pub mod shell;

pub mod external_agent;
pub mod external_issues;
pub mod workflow;

#[cfg(test)]
pub mod test_utils {
    use std::sync::Mutex;
    pub static SET_CWD_MUTEX: Mutex<()> = Mutex::new(());
}
