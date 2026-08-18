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
