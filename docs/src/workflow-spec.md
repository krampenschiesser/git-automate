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
inspects the configured project, runs the workflow pipeline for any issues
that need attention, and reports status back to the project board.

The daemon communicates with two external services:

- **GitHub** via GraphQL for issue discovery, project board management, and
  status updates.
- **OpenCode** via HTTP for agent session creation and lifecycle management.

Configuration is provided through a YAML file (default: `git-automate.yml`)
and supports environment variable interpolation for secret values.
The daemon requires the environment variables `GITHUB_TOKEN`  and `OPENCODE_PW` for
authentication against Github and the OpenCode server.

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
the daemon from continuing to process. The operator
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
cycle. Continuous log spamming is avoided by not repeating the same messages with each new loop.

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
   using the derived project name. The newly created non-global numerical ID is
   written back to `git-automate.yml` so that subsequent runs use the
   persisted ID directly.

3. **Resolve numeric project IDs.** If `projectId` is present and its
   value is purely numeric (e.g. `projectId: 1`), it is treated as a
   project *number* (databaseId), not a relay global ID. At runtime the
    daemon resolves it to the corresponding global node ID via a
    GitHub API project lookup by number. The original numeric value is
   **preserved** in the config file and is never replaced by the
   resolved global ID.

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
6. **Ensure the `waveId` number field** The project must have a `waveId`
   number field to store execution waves for sub-issues.

### Idempotency

Setup is idempotent. An existing project that already has all seven
status options and the `sessionId` field performs no mutations —
queries are still issued to verify the current state, but no create or
delete operations are executed. Missing status
options are added, extra options are removed, and a missing
`sessionId` field is created. No errors are raised in these cases.

### GitHub Token Requirements

- **`serve` mode**: `GITHUB_TOKEN` is required. If it is missing or
  empty, the daemon exits immediately (fail-fast).
- **`doctor` mode**: `GITHUB_TOKEN` is required. If it is missing or
  empty, the daemon exits immediately (fail-fast).

## Step 2: OpenCode Health Check

The OpenCode Health Check runs at **every poll cycle**, unlike Setup
which runs only at startup. Its purpose is to verify that the OpenCode
server is reachable and responsive before the daemon proceeds to the
triage, todo, and review steps.

### What the Check Does

The `check_opencode` function probes the OpenCode server's
health endpoint. This is the **only** endpoint consulted
during this step.

- If the server responds with a healthy status, the check logs an
  informational message and returns `Ok(Workflow::Continue)`.
- If the server is unreachable or reports unhealthy, the check logs a
  warning and returns `Ok(Workflow::Skip)`. The daemon skips all steps of the workflow.

Both the info message and the warning are only printed once until the status changes.
That way we do not get log-spammed with the same message. 


### GitHub Token Requirements

Same as Setup: `GITHUB_TOKEN` is required in `serve` mode (fail-fast)
and required in `doctor` mode (fail-fast).

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
   triage agent handlebars template as the system prompt and the triage prompt
   template as the user message, filling in the following handlebars template
   variables:
   - `ISSUE_TITLE`
   - `ISSUE_NUMBER`
   - `ISSUE_BODY`
   - `ISSUE_LABELS`
   - `ISSUE_ASSIGNEE`

   The returned session ID is written back to the project item's
   `sessionId` field via `updateProjectItemSessionId`. If a `sessionId`
   already exists, no new session is started.

6. **Create sub-tasks with dependencies **: Once the opencode session completes and is not active anymore
   the result should define a possible list (might be empty) of sub-tasks and their interdependencies to create.
   Those subtasks will be created by code calling the github api and setting the **Todo** state on them.
   If no subtasks are needed then main issue will be set into the **Todo** state.

## Step 4: Todo

The Todo step runs at every poll cycle. It finds project items that
have reached "Todo" status but have not yet been assigned a developer
session, and it starts one.

### What Todo Does

1. **Find Todo items without sessions.** The daemon queries all project
   items and selects those whose Status is "Todo" and whose `sessionId`
   field is empty.

2. **Create a branch if needed.** For each selected item, the daemon
   constructs a branch name in the configured format `issue-{N}`, where
   `N` is the issue number (e.g. `issue-42`). If the branch does not
   already exist in the repository, the daemon creates it from the
   repository's default branch. If the item is a sub-issue it creates a branch with the 
   configured format `issue-{N}-sub-{M}`.

3. **Filter issues by waveId**: Sub-issues have an optional integer field `waveId`.
   Sub-issues can only start if there either is no lower wave id on any sub-issue or all sub-issues with a lower wave id are in status **Done**
   Sub-issues that cannot start due to their waveId will be filtered out and not started.

4. **Gather PR comments**: if a pull request for the branch exists it checks for any unresolved comments.
   It then generates a list of comments, their files and the line numbers to pass down to the developer session.

5. **Start a developer session.** The daemon starts a developer OpenCode
   session (code-driven) using the developer agent template as the
   system prompt and the developer prompt template as the user message.
   Template variables filled in are:
   - `ISSUE_TITLE`
   - `ISSUE_NUMBER`
   - `ISSUE_BODY`
   - `BRANCH_NAME`
   - `PROJECT_REPOSITORY`
   - `PR_URL`
   - `PR_CHANGES`
   - `PR_COMMENTS`

6. **Write the session ID.** The returned session ID is written to the
   project item's `sessionId` field via `updateProjectItemSessionId`.

### Branch Name Invariant

The branch name format `issue-{N}` is the default and can be overwritten by a value in the configuration.
For sub issues the format `issue-{N}-sub-{M}` is used.

## Status Flow

The workflow defines seven statuses that an issue item passes through
in sequence. Status transitions are performed by code, never by LLM
agents.

### The Seven Statuses

| # | Status           | Entered When                                                         |
|---|------------------|----------------------------------------------------------------------|
| 1 | Triage           | A new `@ai`-tagged issue is added to the project by the triage step. |
| 2 | Todo             | The triage session completes.                                        |
| 3 | In Development   | A developer session is running for the item.                         |
| 4 | Review Technical | The developer session completes and a reviewer session starts.       |
| 5 | Review Product   | The technical review completes and a product review session starts.  |
| 6 | QA               | The product review completes and a QA session starts.                |
| 7 | Done             | The QA session completes successfully.                               |

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
"Todo". 

**QA sends back.** If the QA agent determines that the work does not
meet acceptance criteria, the item transitions from "QA" back to
"Todo". 

**Technical review sends back.** If the Technical review agent determines that the work does not
meet acceptance criteria, the item transitions from "Review Technical" back to
"Todo". 

**Failed review recovery.** If a review session (technical, product, or
QA) completes without producing a status transition, the daemon detects
this condition on the next poll cycle and recovers automatically:
the item is reset to "Todo", its `sessionId` is cleared and a new
developer session is started. This prevents items from being stranded in a review
status with a completed but unrecorded session.

### Key Principle

At no point do LLM agents perform status transitions. All transitions
are driven by the daemon's code, which observes session completion and
updates the project board accordingly. The agents produce output and
feedback; the daemon interprets that output and moves the item forward.

## Step 5: Review

Review runs at every poll cycle, after Setup (startup only), the
OpenCode Health Check, Triage, and Todo. Its job is to start review
sessions for project items that have reached any of the three review
statuses and do not yet have an active session.

### Review States

The review step processes three states **in order**:

1. **Review Technical**
2. **Review Product**
3. **QA**

For each state, the daemon finds all project items whose `Status` matches
that state and whose `sessionId` field is empty. If no items match a
given state, that state is skipped entirely.

### Agent and Template Mapping

Each review state uses a specific agent and prompt template:

| State            | Agent    | Prompt Template |
|------------------|----------|-----------------|
| Review Technical | Reviewer | `reviewer`      |
| Review Product   | Product  | `product`       |
| QA               | QA       | `qa`            |

The agent template is loaded as the system prompt and the prompt
template is filled with variables and used as the user message. Both
loads are code-driven; no LLM agent decides which template to use.

### Agent Decision Outcomes

Each review agent can produce one of three outcomes. The daemon
monitors the session and advances the project item's status once the
agent completes:

| State            | Agent    | Outcome: Approve             | Outcome: Request Changes                                               |
|------------------|----------|------------------------------|------------------------------------------------------------------------|
| Review Technical | Reviewer | Advances to "Review Product" | Reverts to "In Development" (agent provides feedback on pull request)  |
| Review Product   | Product  | Advances to "QA"             | Returns to "In Development" (agent provides feedback on pull request)  |
| QA               | QA       | Advances to "Done"           | Returns to "In Development" (agent provides findings  on pull request) |

"Request changes" results in the status moving backward to "Todo", while "approve" advances forward. 
Any findings are commented on the pull request created by completion of the "In development" status.

### Template Variables

Review states share a common set of six template variables:

- `ISSUE_NUMBER` — the GitHub issue number
- `BRANCH_NAME` — the feature branch (e.g. `issue-42`)
- `ISSUE_TITLE` — the issue title, or `"Issue #N"` as fallback
- `ISSUE_BODY` — the issue body text, or empty string as fallback
- `PR_URL` — the pull request URL (starts empty; populated by the developer agent)
- `PR_CHANGES` — the PR diff or change description (starts empty)
- `PR_COMMENTS` — comments left by reviewers, need to be resolved by the developer agent

`PR_URL` and `PR_CHANGES` are initialized to empty strings when first handed off to the developer agent.
When handing off to the first reviewer they need to be filled.

The `PR_COMMENTS` are populated by code in the *TODO* state before handing off to the developer agent
and the "In Development" phase.
They are resolved after the developer agent finished. 
The resolution step is ai and code-driven. 
A new llm reads the developers agent responses and extracts which comments to resolve.
Then code-driven api calls resolve the comments so that they disappear from future prompts.

### Session Creation

For each matching item, the daemon creates an OpenCode session
(code-driven) with the loaded system prompt and filled user prompt.
The session title is formatted as:

```
"{status}: {issue_title}"
```

For example: `"Review Technical: @ai Add dark mode support"`. If the
issue cannot be found in the issue map, the title falls back to:

```
"{status}: #{issue_number}"
```

For example: `"Review Technical: #42"`.

After the session is created, its ID is written to the project item's
`sessionId` field.

### Failed Review Recovery

After all three review states have been processed, the review step
calls `run_failed_review_check` to perform recovery. This is an
**integral part of the review step**, not an optional or separate
phase.

#### What Failed Review Recovery Detects

The recovery check identifies review sessions that have completed
(i.e. they no longer appear in OpenCode's active session list) but
whose project item status has **not** transitioned. This indicates
that the review agent finished without producing the expected status
change, leaving the item stranded.

#### Recovery Procedure

For each stuck item found in any of the three review states, the
recovery check performs the following steps in order:

1. **Clear the `sessionId` field.** The existing session ID is removed
   via `updateProjectItemSessionId` with a `None` value.
2. **Start a new developer session.** The daemon loads the developer
   agent template as the system prompt and the developer prompt
   template as the user message, filling in the standard developer
   variables (`ISSUE_TITLE`, `ISSUE_NUMBER`, `ISSUE_BODY`,
   `BRANCH_NAME`, `PROJECT_REPOSITORY`). This session creation is
   code-driven.
3. **Set status to "Todo".** The item's Status field is
   advanced to "Todo" via `updateProjectItemStatus`.

This recovery mechanism ensures that a review agent failure does not
permanently block an issue. The item re-enters the development cycle
with a fresh session, giving the developer agent another opportunity
to produce a correct implementation.

## Concurrency

The daemon limits the total number of concurrently active OpenCode
agent sessions by a map of model to concurrency in `git-automate.yml`:
```
opencode:
  concurrency:
    myprovider/slow: 2
    myprovider/fast: 4
```


### Behavior When Set

When `concurrency` is configured, it acts as a **hard cap**. Before
creating any new OpenCode session, the daemon queries the OpenCode
server for the current active sessions. For each session it resolves the model
handling the session. 
If the sum of active sessions for any model is greater than the limit set, session creation is skipped completely and therefore
ending the current loop.
An information message is logged once for this. Subsequent skips of the workflow loop will not be logged.
However, when the workflow continues processing a new info message needs to be logged informing the user about capacity being available again.

## Agent Catalog

The git-automate workflow defines six agents. Each agent has
an embedded definition file that is copied to `~/.config/git-automate/agents`
by the initial setup.

| Agent | File | Role | Invoked By |
|---|---|---|---|
| Triage | `git-automate-triage.agent.md` | Analyze issues, break into sub-tasks, and transition items to "Todo" | Step 3 (Triage) |
| Developer | `git-automate-developer.agent.md` | Implement code changes for triaged issues | Step 4 (Todo), Failed Review Recovery |
| Reviewer | `git-automate-reviewer.agent.md` | Technical code review for pull requests | Step 5 (Review Technical) |
| Product | `git-automate-product.agent.md` | Product and UX review for implemented features | Step 5 (Review Product) |
| QA | `git-automate-qa.agent.md` | Testing and QA verification before final approval | Step 5 (QA) |

## Prompt Variable Catalog

Each prompt template expects a specific set of variables. The daemon
fills these variables at runtime before passing the template to an
OpenCode session. The table below lists each template by filename and
its expected variables.

| Template                 | Variables                                                                                                               |
|--------------------------|-------------------------------------------------------------------------------------------------------------------------|
| `prompts/triage.md`      | `ISSUE_TITLE`, `ISSUE_NUMBER`, `ISSUE_BODY`, `ISSUE_LABELS`, `ISSUE_ASSIGNEE`                                           |
| `prompts/developer.md`   | `ISSUE_TITLE`, `ISSUE_NUMBER`, `ISSUE_BODY`, `BRANCH_NAME`, `PROJECT_REPOSITORY`, `PR_URL`, `PR_CHANGES`, `PR_COMMENTS` |
| `prompts/reviewer.md`    | `ISSUE_NUMBER`, `PR_URL`, `BRANCH_NAME`, `ISSUE_TITLE`, `PR_CHANGES`                                                    |
| `prompts/product.md`     | `ISSUE_NUMBER`, `PR_URL`, `ISSUE_TITLE`, `ISSUE_BODY`, `PR_CHANGES`                                                     |
| `prompts/qa.md`          | `ISSUE_NUMBER`, `PR_URL`, `ISSUE_TITLE`, `BRANCH_NAME`                                                                  |
| `prompts/taskmanager.md` | `PROJECT_NAME`, `PROJECT_BOARD_URL`, `ISSUE_LIST`                                                                       |

Note: The `taskmanager.md` template variables are listed for
completeness but are never populated at runtime, since the TaskManager
agent is not invoked.


## Doctor

The `doctor` command is a one-shot setup wizard.
The doctor command reuses the setup code from the workflow loop.
It is invoked with:

```bash
git-automate doctor --config git-automate.yml
```
