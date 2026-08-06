# git-automate

A standalone Rust daemon that automates GitHub issue workflows via OpenCode agent sessions.

## Overview

git-automate polls GitHub for `@ai`-tagged issues, assigns them to GitHub Projects V2 boards,
and starts OpenCode agent sessions (triage -> developer -> review -> QA) to process them.

## Installation

```bash
cargo install --path .
```

## Usage

```bash
# Run the daemon (polls every 30s)
git-automate serve --config git-automate.yml

# Check OpenCode server health
git-automate health --url http://localhost:8081 --pw <password>
```

## Configuration

The config file (`git-automate.yml`) defines projects to monitor:

```yaml
projects:
  my-project:
    repository: https://github.com/owner/repo
    projectId: 1
    titlePattern: '@ai.*'
    directory: /path/to/repo
    opencode:
      url: http://localhost:8081
      pw: ${env:OPENCODE_PW}
```

### Environment Variables

- `GITHUB_TOKEN` -- GitHub API token. If unset, GitHub operations are skipped.
- `OPENCODE_PW` -- OpenCode server password (referenced in config via `${env:OPENCODE_PW}`).

### Config Fields

| Field            | Required | Description                                      |
|------------------|----------|--------------------------------------------------|
| `concurrency`    | no       | Limits total active OpenCode agent sessions. When set, skips session creation if active session count >= limit (default: no limit) |
| `repository`     | yes      | GitHub repo URL or `owner/repo` shorthand        |
| `projectId`      | no       | GitHub Project V2 ID (created automatically if absent) |
| `directory`      | no       | Working directory for OpenCode agent sessions     |
| `opencode.url`   | yes      | OpenCode server base URL                         |
| `titlePattern`   | no       | Regex to match issue titles for triage (default `@ai.*`) |
| `opencode.pw`    | yes      | OpenCode server password (use `${env:VAR}`)      |

## Workflow

1. **Triage** -- Issues titled `@ai ...` are added to the project board and assigned "Triage" status.
2. **Todo** -- Items with "Todo" status get a `issue-{N}` branch and a developer session starts.
3. **Review** -- Items in "Review Technical", "Review Product", or "QA" status get prompt templates filled and review sessions started.

Status flow: `Triage -> Todo -> In Development -> Review Technical -> Review Product -> QA -> Done`

## Development

```bash
cargo build                  # Build the binary
cargo test                   # Run all tests
cargo clippy -- -D warnings  # Lint
cargo fmt --check            # Check formatting
```

## License

ISC
