# Workflow

The workflow is a seven-stage state machine that an issue passes through from discovery to completion.

## Status flow

```
Triage → Todo → In Development → Review Technical → Review Product → QA → Done
```

Each status is a single-select option on the GitHub Project V2 board. The daemon ensures all seven options exist on first setup.

## Stage descriptions

### Triage

Triggered when an issue title matches the `titlePattern` regex (default: `@ai.*`).

Actions:
1. Add the issue to the project board (if not already present).
2. Set status to `Triage`.
3. Start a `git-automate-triage` agent session with the issue details.
4. Store the session ID in the project item's `sessionId` field.

The triage agent analyzes the issue, breaks it into sub-tasks, creates todos, and transitions the issue to `Todo`.

### Todo

Triggered when a project item has `Status == Todo` and no session ID.

Actions:
1. Clone the repository to `/tmp/git-automate-work/{owner}-{repo}` (if not already cloned).
2. Create branch `issue-{N}` from the default branch (if it doesn't exist).
3. Start a `git-automate-developer` agent session.
4. Store the session ID.

The developer agent implements the code changes, runs tests, opens a PR, and transitions the issue to `Review Technical`.

### In Development

This status is set by the developer agent after the PR is opened. The daemon does not actively manage this stage — it is a pass-through.

### Review Technical

Triggered when `Status == Review Technical` and no session ID exists.

Actions:
1. Load the `reviewer` prompt template.
2. Fill placeholders (`{{ISSUE_NUMBER}}`, `{{BRANCH_NAME}}`, `{{ISSUE_TITLE}}`, `{{ISSUE_BODY}}`, `{{PR_URL}}`, `{{PR_CHANGES}}`).
3. Start a `git-automate-reviewer` agent session.
4. Store the session ID.

The reviewer agent checks code quality, type safety, and test coverage. It either approves (→ `Review Product`) or requests changes (stays in `Review Technical`).

### Review Product

Triggered when `Status == Review Product` and no session ID exists.

Actions: same pattern as Review Technical, using the `product` prompt and `git-automate-product` agent.

The product agent verifies requirements and UX. It either approves (→ `QA`) or requests changes (→ `In Development`).

### QA

Triggered when `Status == QA` and no session ID exists.

Actions: same pattern, using the `qa` prompt and `git-automate-qa` agent.

The QA agent runs the test suite, performs manual testing, and checks edge cases. It either approves (→ `Done`) or sends back (→ `In Development`).

### Done

The final status. No agent session is started. The issue is complete.

## Idempotency

The daemon is idempotent. For each check, it skips items that already have a `sessionId` value. Re-running the daemon is safe and will not create duplicate sessions or branches.

## Concurrency

The `concurrency` config field limits the total number of active OpenCode sessions across all projects. When the limit is reached, session creation is skipped with a warning log.
