# Introduction

`git-automate` is a standalone Rust daemon that automates the lifecycle of GitHub issues through AI agent sessions. It bridges GitHub Projects V2 and the OpenCode agent platform to create a fully automated issue-to-PR pipeline.

## What it does

1. **Polls** GitHub every 30 seconds for issues whose titles match a configurable pattern (default: `@ai.*`).
2. **Assigns** matching issues to a GitHub Project V2 board, creating the project automatically if it doesn't exist.
3. **Starts OpenCode agent sessions** that process each issue through a seven-stage workflow:

   ```
   Triage → Todo → In Development → Review Technical → Review Product → QA → Done
   ```

4. **Tracks progress** by storing OpenCode session IDs in a custom `sessionId` field on each project item.

## Key design principles

- **Fail-fast on startup**: missing `GITHUB_TOKEN` or an unreadable config file causes an immediate exit.
- **Error isolation**: each workflow step catches and logs its own errors — a failure in one project never blocks another.
- **Idempotent**: re-running the daemon is safe; it skips items that already have a session.
- **Provider-agnostic abstractions**: the `ExternalAgent` trait and `ExternalIssueSource` trait make it straightforward to add new agent platforms or issue trackers.
- **Embedded assets**: prompt templates and agent definitions are compiled into the binary via `include_str!`, requiring no runtime file access.

## Quick start

```bash
# Install
cargo install --path .

# Run (requires GITHUB_TOKEN and OPENCODE_PW)
GITHUB_TOKEN=ghp_xxx OPENCODE_PW=secret git-automate serve
```

See [Getting Started](./getting-started.md) for full setup instructions.
