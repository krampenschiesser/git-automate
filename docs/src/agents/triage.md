# Triage Agent

**Agent name:** `git-automate-triage`  
**Prompt template:** `src/assets/prompts/triage.md`  
**Workflow stage:** Triage

## Role

The triage agent is the first agent in the pipeline. It receives a new GitHub issue and is responsible for:

1. Analyzing the issue body and any linked resources.
2. Identifying the core problem and acceptance criteria.
3. Breaking the work into discrete, trackable sub-tasks.
4. Creating todos for each sub-task.
5. Posting clarifying questions if the issue is ambiguous.
6. Transitioning the issue to `Todo` status.

## Input

The prompt is filled with:

| Placeholder | Source |
|-------------|--------|
| `{{ISSUE_TITLE}}` | Issue title |
| `{{ISSUE_NUMBER}}` | Issue number |
| `{{ISSUE_BODY}}` | Issue body (or `Issue #{N}: {title}` if body is null) |
| `{{ISSUE_LABELS}}` | Issue labels |
| `{{ISSUE_ASSIGNEE}}` | Issue assignee |

## Expected output

A summary containing:
- Restatement of the issue's requirements
- List of sub-tasks identified
- Any clarifying questions asked
- Confirmation that todos were created and the issue was transitioned to `Todo`
