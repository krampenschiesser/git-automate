# Developer Agent

**Agent name:** `git-automate-developer`  
**Prompt template:** `src/assets/prompts/developer.md`  
**Workflow stage:** Todo → In Development

## Role

The developer agent implements code changes to resolve a triaged issue:

1. Creates a feature branch `issue-{N}` from the default branch.
2. Reviews acceptance criteria and sub-tasks from the triage step.
3. Implements minimum code changes following project conventions.
4. Runs the test suite and adds tests for new behavior.
5. Commits with descriptive, atomic messages.
6. Pushes the branch and opens a pull request referencing the issue.
7. Transitions the issue to `Review Technical`.

## Input

| Placeholder | Source |
|-------------|--------|
| `{{ISSUE_TITLE}}` | Issue title |
| `{{ISSUE_NUMBER}}` | Issue number |
| `{{ISSUE_BODY}}` | Issue body |
| `{{BRANCH_NAME}}` | `issue-{N}` |
| `{{PROJECT_REPOSITORY}}` | Full repository URL |

## Expected output

A summary containing:
- The branch created
- Files changed
- Test results
- The pull request URL
- Confirmation that the issue was transitioned to `Review Technical`

## Branch naming

Branches are named `issue-{ISSUE_NUMBER}` (e.g., `issue-42`). The daemon creates the branch if it does not exist before starting the agent session.
