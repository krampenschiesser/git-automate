---
slug: workflow-spec
status: draft
intent: |
  Formal specification of the daemon loop and workflow orchestration
  for the git-automate binary. Documents the polling cycle, error
  isolation model, and shutdown behavior.
---

# Workflow Specification

## Overview

`git-automate` is a standalone Rust daemon that automates the end-to-end
lifecycle of GitHub issues tagged with `@ai`. It polls a configured GitHub
repository for new or updated `@ai`-tagged issues, classifies each issue
through a multi-agent workflow, and records progress on a GitHub Projects
V2 board.

The binary does not act as a library or SDK. It is a self-contained service
that runs continuously, driven by a 30-second polling loop. Each iteration
inspects all configured projects, runs the workflow pipeline for any issues
that need attention, and reports status back to the project board.

The daemon communicates with two external services:

- **GitHub** via GraphQL for issue discovery, project board management, and
  status updates.
- **OpenCode** via HTTP for agent session creation and lifecycle management.

Configuration is provided through a YAML file (default: `git-automate.yml`)
and supports environment variable interpolation via the `${env:VAR}` syntax.
The daemon requires a `GITHUB_TOKEN` to operate and uses `OPENCODE_PW` for
authentication against the OpenCode server.

## Daemon Loop

The daemon runs a single continuous loop controlled by the `serve` command.
On startup, it performs an immediate evaluation of all configured projects
before entering the regular polling cycle.

### Poll Interval

The daemon polls GitHub every **30 seconds**. After each polling cycle
completes, the timer resets for the next 30-second interval. There is no
backoff or jitter; the interval is fixed.

### Run Cycle

Each cycle invokes `run_all()`, which iterates over all projects defined in
the configuration and executes the full workflow pipeline for each one.

`run_all()` **always returns `Ok(())`**. Errors from individual workflow
steps are never propagated to the caller. Instead, each failure is logged
with the pattern:

```
"<check> failed for {project}: {e}"
```

This design ensures that a single failing project or step never prevents
the daemon from continuing to process the remaining projects. The operator
should inspect logs to diagnose issues, not the return value of the daemon.

### Startup vs. Polling Runs

The first execution of `run_all()` occurs immediately at startup, before
the first 30-second timer begins. If this startup run encounters errors,
they are logged as:

```
Startup runAll failed
```

Subsequent executions during the polling loop log errors as:

```
Polling runAll failed
```

In both cases, the daemon continues running and will retry on the next
cycle.

### One-Shot Mode

The `--once` flag causes the daemon to execute a single cycle and then
exit. This is useful for testing, manual runs, or integration with
external schedulers such as cron.

### Shutdown

The daemon handles `SIGINT` and `SIGTERM` gracefully. When either signal
is received, the current cycle completes, and the daemon exits with the
log message:

```
Received shutdown signal, exiting
```

No in-progress workflow steps are forcibly terminated. The daemon waits
for the current cycle to finish before exiting.

## Step 1: Setup

Setup is the first workflow step. It runs **only at startup**, not on
every poll cycle. This is a deliberate divergence from the original
design which executed setup on each iteration; running it once at
startup is sufficient because the GitHub Project V2 board structure
does not change during normal operation.

### What Setup Does

1. **Parse the repository URL.** The `git.repository` field (e.g.
   `https://github.com/owner/repo`) is parsed to extract the owner and
   repository name. The project name is derived from the repository
   name.

2. **Create the GitHub Project V2 if absent.** If `projectId` is not
   present in the configuration, a new project is created on GitHub
   using the derived project name. The newly created global ID is
   written back to `git-automate.yml` so that subsequent runs use the
   persisted ID directly.

3. **Resolve numeric project IDs.** If `projectId` is present and its
   value is purely numeric (e.g. `projectId: 1`), it is treated as a
   project *number* (databaseId), not a relay global ID. At runtime the
   daemon resolves it to the corresponding global node ID via the
   `projectV2(number:)` GraphQL query. The original numeric value is
   **preserved** in the config file and is never replaced by the
   resolved global ID. If `projectId` is already a non-numeric global
   ID, it is used as-is with no network call.

4. **Ensure status options.** The project's Status field must contain
   all seven `WorkflowStatus` values as options:

   - Triage
   - Todo
   - In Development
   - Review Technical
   - Review Product
   - QA
   - Done

   Any existing status option whose name does not match one of these
   seven values is **removed**. This keeps the board clean and prevents
   stale options from accumulating. Missing options are added as
   needed.

5. **Ensure the `sessionId` field.** The project must have a `sessionId`
   TEXT field. If the field does not exist, it is created. This field
   stores the OpenCode session identifier for each project item.

### Idempotency

Setup is idempotent. An existing project that already has all seven
status options and the `sessionId` field is a no-op. Missing status
options are added, extra options are removed, and a missing
`sessionId` field is created. No errors are raised in these cases.

### GitHub Token Requirements

- **`serve` mode**: `GITHUB_TOKEN` is required. If it is missing or
  empty, the daemon exits immediately (fail-fast).
- **`doctor` mode**: `GITHUB_TOKEN` is optional. If missing, a warning
  is logged and all GitHub-dependent checks (including setup) are
  skipped.

## Step 2: OpenCode Health Check

The OpenCode Health Check runs at **every poll cycle**, unlike Setup
which runs only at startup. Its purpose is to verify that the OpenCode
server is reachable and responsive before the daemon proceeds to the
triage, todo, and review steps.

### What the Check Does

The `check_opencode` function probes the OpenCode server's
`/global/health` endpoint. This is the **only** endpoint consulted
during this step.

- If the server responds with a healthy status, the check logs an
  informational message and returns `Ok(())`.
- If the server is unreachable or reports unhealthy, the check logs a
  warning and returns `Ok(())`. The daemon continues to the next step
  rather than aborting, because transient OpenCode outages should not
  halt the entire polling loop.

### Agent Verification (Obsolete)

The current implementation queries the OpenCode `/agent` endpoint and
checks that all six required agents are installed. **This check is
obsolete and is not part of the specification.** The spec does not
require verifying agent installation at this stage. Agent readiness is
assumed; missing agents will surface as session-creation errors in
later steps.

### GitHub Token Requirements

Same as Setup: `GITHUB_TOKEN` is required in `serve` mode (fail-fast)
and optional in `doctor` mode (warns and skips).

## Step 3: Triage

Triage runs at every poll cycle, after Setup (which runs only at startup)
and the OpenCode Health Check. Its job is to discover new `@ai`-tagged
issues in the configured GitHub repository and bring them into the
project board.

### What Triage Does

1. **List repository issues.** The daemon queries the GitHub REST API for
   all open issues in the configured repository.

2. **Filter by `titlePattern`.** The default pattern is `@ai.*`, meaning
   issue titles must begin with `@ai` (followed by any characters). The
   pattern is compiled as a regex at the start of each triage cycle.
   Issues whose titles do not match the pattern are ignored entirely.
   This is the `@ai` filter applied at the issue title level.

3. **Add matching issues to the project.** For each issue that matches
   the pattern, the daemon checks whether the issue already has a
   corresponding item in the GitHub Project V2 board. If not, it adds
   the issue to the project via `addIssueToProject`.

4. **Set status to "Triage".** For every matching issue (whether newly
   added or already present), the daemon sets the item's Status field
   to "Triage" via `updateProjectItemStatus`. These status transitions
   are performed by code, not by LLM agents.

5. **Start a triage session if needed.** If the project item does not
   yet have a `sessionId` value, the daemon starts a triage OpenCode
   session. This session creation is code-driven: the daemon loads the
   triage agent template as the system prompt and the triage prompt
   template as the user message, filling in the following template
   variables:
   - `ISSUE_TITLE`
   - `ISSUE_NUMBER`
   - `ISSUE_BODY`
   - `ISSUE_LABELS`
   - `ISSUE_ASSIGNEE`

   The returned session ID is written back to the project item's
   `sessionId` field via `updateProjectItemSessionId`. If a `sessionId`
   already exists, no new session is started.

### What Triage Does Not Do

- Triage does not invoke any LLM agent to decide which issues to
  process. The `@ai` regex filter and the project addition logic are
  entirely code-driven.
- Triage does not perform any status transitions beyond setting the
  initial "Triage" status. All subsequent status changes are handled
  by later workflow steps.

## Step 4: Todo

The Todo step runs at every poll cycle. It finds project items that
have reached "Todo" status but have not yet been assigned a developer
session, and it starts one.

### What Todo Does

1. **Find Todo items without sessions.** The daemon queries all project
   items and selects those whose Status is "Todo" and whose `sessionId`
   field is empty.

2. **Create a branch if needed.** For each selected item, the daemon
   constructs a branch name in the hardcoded format `issue-{N}`, where
   `N` is the issue number (e.g. `issue-42`). If the branch does not
   already exist in the repository, the daemon creates it from the
   repository's default branch.

3. **Start a developer session.** The daemon starts a developer OpenCode
   session (code-driven) using the developer agent template as the
   system prompt and the developer prompt template as the user message.
   Template variables filled in are:
   - `ISSUE_TITLE`
   - `ISSUE_NUMBER`
   - `ISSUE_BODY`
   - `BRANCH_NAME`
   - `PROJECT_REPOSITORY`

   If the issue is not found in the issue map, the title falls back to
   `"Dev work for issue #{N}"` and the message falls back to
   `"Issue #{N}"`.

4. **Write the session ID.** The returned session ID is written to the
   project item's `sessionId` field via `updateProjectItemSessionId`.

### Branch Name Invariant

The branch name format `issue-{N}` is a hardcoded invariant. It is not
read from configuration and is not influenced by the issue title or any
other external input. Every developer session for issue `N` uses the
branch `issue-{N}`.

## Status Flow

The workflow defines seven statuses that an issue item passes through
in sequence. Status transitions are performed by code, never by LLM
agents.

### The Seven Statuses

| # | Status | Entered When |
|---|---|---|
| 1 | Triage | A new `@ai`-tagged issue is added to the project by the triage step. |
| 2 | Todo | The triage session completes and the developer session starts. |
| 3 | In Development | A developer session is running for the item. |
| 4 | Review Technical | The developer session completes and a reviewer session starts. |
| 5 | Review Product | The technical review completes and a product review session starts. |
| 6 | QA | The product review completes and a QA session starts. |
| 7 | Done | The QA session completes successfully. |

### Normal Flow

```
Triage → Todo → In Development → Review Technical → Review Product → QA → Done
```

Each arrow represents a code-driven status transition triggered when the
corresponding OpenCode session completes and the next step in the
pipeline detects the completion.

### Exception Paths

**Product review sends back.** If the product reviewer determines that
changes are needed, the item transitions from "Review Product" back to
"In Development" (not back to "Todo"). A new developer session is
started to address the feedback.

**QA sends back.** If the QA agent determines that the work does not
meet acceptance criteria, the item transitions from "QA" back to
"In Development". A new developer session is started to address the
issues found.

**Failed review recovery.** If a review session (technical, product, or
QA) completes without producing a status transition, the daemon detects
this condition on the next poll cycle and recovers automatically:
the item is reset to "Todo", its `sessionId` is cleared, a new
developer session is started, and the status is advanced to
"In Development". This prevents items from being stranded in a review
status with a completed but unrecorded session.

### Key Principle

At no point do LLM agents perform status transitions. All transitions
are driven by the daemon's code, which observes session completion and
updates the project board accordingly. The agents produce output and
feedback; the daemon interprets that output and moves the item forward.
