# Task Manager Agent

**Agent name:** `git-automate-taskmanager`  
**Prompt template:** `src/assets/prompts/taskmanager.md`  
**Workflow stage:** N/A (on-demand / manual)

## Role

The task manager agent is a coordination agent that audits the project board:

1. Verifies all issues are associated with the correct project board.
2. Reviews current status of each issue and updates status fields.
3. Confirms each issue has an assignee; flags unassigned issues.
4. Identifies stalled issues (no status change in 7+ days) or orphaned tasks.
5. Coordinates transitions between workflow stages.

## Input

| Placeholder | Source |
|-------------|--------|
| `{{PROJECT_NAME}}` | Project board name |
| `{{PROJECT_BOARD_URL}}` | URL to the project board |
| `{{ISSUE_LIST}}` | Comma-separated list of issue numbers to audit |

## Expected output

A summary containing:
- Any status changes applied
- Issues missing project association or assignees
- Stalled or orphaned issues
- Coordination notes for the next workflow step

## Usage

The task manager agent is not automatically dispatched by the daemon's polling loop. It is available for manual invocation or future automation hooks.
