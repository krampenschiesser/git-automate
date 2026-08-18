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
