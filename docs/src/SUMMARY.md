# Book

[![CI](https://github.com/krampenschiesser/git-automate/actions/workflows/rust.yml/badge.svg)](https://github.com/krampenschiesser/git-automate/actions/workflows/rust.yml)

A standalone Rust daemon that automates GitHub issue workflows via OpenCode agent sessions. It polls GitHub for `@ai`-tagged issues, assigns them to GitHub Projects V2 boards, and starts OpenCode agent sessions to triage, develop, review, and QA each issue.

---

- [CLI](./cli.md)
- [Getting Started](./getting-started.md)
- [Configuration](./configuration.md)
- [Architecture](./architecture/overview.md)
  - [Daemon Lifecycle](./architecture/daemon-lifecycle.md)
  - [Workflow Engine](./architecture/workflow-engine.md)
  - [External Integrations](./architecture/external-integrations.md)
- [Workflow](./workflow.md)
- [Agents](./agents/overview.md)
  - [Triage](./agents/triage.md)
  - [Task Manager](./agents/task-manager.md)
  - [Developer](./agents/developer.md)
  - [Reviewer](./agents/reviewer.md)
  - [Product](./agents/product.md)
  - [QA](./agents/qa.md)
- [API Reference](./api/overview.md)
  - [CLI](./api/cli.md)
  - [Config](./api/config.md)
  - [WorkflowError](./api/workflow-error.md)
- [Development](./development.md)
