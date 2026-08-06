# Agents Overview

git-automate uses six OpenCode agent definitions, each with a dedicated prompt template. All are embedded in the binary at compile time.

## Agent registry

| Agent name | Prompt template | Workflow stage | Role |
|------------|----------------|----------------|------|
| `git-automate-triage` | `triage.md` | Triage | Analyze issue, create sub-tasks, transition to Todo |
| `git-automate-taskmanager` | `taskmanager.md` | N/A (on-demand) | Audit project board, coordinate status transitions |
| `git-automate-developer` | `developer.md` | Todo | Implement code changes, open PR, transition to Review Technical |
| `git-automate-reviewer` | `reviewer.md` | Review Technical | Review code quality, type safety, tests |
| `git-automate-product` | `product.md` | Review Product | Verify requirements and UX |
| `git-automate-qa` | `qa.md` | QA | Run tests, manual testing, approve or send back |

## Required agents

The daemon verifies that all six agents are installed on startup (during `OpencodeCheck`). Missing agents produce a warning but do not prevent the daemon from running.

```rust
pub const REQUIRED_AGENTS: [AgentName; 6] = [
    AgentName::Triage,
    AgentName::TaskManager,
    AgentName::Developer,
    AgentName::Reviewer,
    AgentName::Product,
    AgentName::QA,
];
```

## Prompt template placeholders

Each prompt template uses `{{KEY}}` placeholders filled by `fill_prompt()` at runtime:

| Placeholder | Description |
|-------------|-------------|
| `{{ISSUE_NUMBER}}` | GitHub issue number |
| `{{ISSUE_TITLE}}` | Issue title |
| `{{ISSUE_BODY}}` | Issue body text |
| `{{ISSUE_LABELS}}` | Issue labels |
| `{{ISSUE_ASSIGNEE}}` | Issue assignee |
| `{{BRANCH_NAME}}` | Feature branch name (`issue-{N}`) |
| `{{PR_URL}}` | Pull request URL (empty until PR is opened) |
| `{{PR_CHANGES}}` | PR diff / changes (populated by developer) |
| `{{PROJECT_NAME}}` | Project board name |
| `{{PROJECT_BOARD_URL}}` | URL to the project board |
| `{{ISSUE_LIST}}` | Comma-separated list of issue numbers |
| `{{PROJECT_REPOSITORY}}` | Full repository URL |

## Agent definition format

Agent files use YAML frontmatter embedded via `include_str!`:

```yaml
---
name: git-automate-triage
description: Triage agent for git-automate
mode: subagent
---
```

The six agent files are:

- `src/assets/agents/git-automate-triage.agent.md`
- `src/assets/agents/git-automate-taskmanager.agent.md`
- `src/assets/agents/git-automate-developer.agent.md`
- `src/assets/agents/git-automate-reviewer.agent.md`
- `src/assets/agents/git-automate-product.agent.md`
- `src/assets/agents/git-automate-qa.agent.md`

## Adding a new agent

1. Create a new file in `src/assets/agents/` with YAML frontmatter.
2. Create a corresponding prompt template in `src/assets/prompts/`.
3. Add the agent name to `REQUIRED_AGENTS` in `src/workflow/mod.rs`.
4. Add the prompt name to `load_prompt_template()` in `src/workflow/helpers.rs`.
5. Add the agent mapping in `ReviewState` (if applicable) or in the relevant check function.
