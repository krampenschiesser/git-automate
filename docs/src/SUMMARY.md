# Book

[![CI](https://github.com/krampenschiesser/git-automate/actions/workflows/rust.yml/badge.svg)](https://github.com/krampenschiesser/git-automate/actions/workflows/rust.yml)

A standalone Rust daemon that automates GitHub issue workflows via OpenCode agent sessions. It polls GitHub for `@ai`-tagged issues, assigns them to GitHub Projects V2 boards, and starts OpenCode agent sessions to triage, develop, review, and QA each issue.

---

- [Workflow spec](./workflow-spec.md)