//! git-automate: A standalone Rust daemon that automates GitHub issue workflows
//! via OpenCode agent sessions.

pub mod config;
pub mod log;
pub mod shell;

pub mod github;
pub mod opencode;
pub mod workflow;

#[cfg(test)]
pub mod test_utils {
    use std::sync::Mutex;
    pub static SET_CWD_MUTEX: Mutex<()> = Mutex::new(());
}
