# sks pipeline: install and runtime configuration

The `sks-*` agents are a 14-agent pipeline for opencode. You talk to one primary
agent, `sks-orchestrate`, and it routes the request to a specialist. The pipeline plans,
implements, reviews, and commits work, and every step leaves evidence behind.

This file covers four things:

1. The agent roster and what each agent does.
2. How to install the definitions into opencode.
3. The `opencode.json` config the pipeline needs to run nested agents.
4. What breaks when that config is missing.

Companion references in this directory:

- `CONTRACTS.md` owns the delegation, permission, and routing contract. Read it before
  you change any agent.
- `PLAN-SCHEMA.md` owns the on-disk JSON plan shape.
- The opencode v2 docs own the runtime facts: https://opencode.ai/v2/docs/tools/#subagent
  (the `subagent` tool) and https://opencode.ai/v2/docs/agents (agent frontmatter and
  permissions). The v2 source file `packages/core/src/tool/plugin/subagent.ts` is the
  authoritative schema; section 6 pins it.

## 1. Agent roster

Every agent is a Markdown file with YAML frontmatter. `sks-orchestrate` is the only
`primary` agent; the other 13 are `subagent`. Nothing else in this directory is an
agent.

| Agent | `mode` | Purpose |
|---|---|---|
| `sks-orchestrate` | primary | Primary entry point. Classifies each request into exactly one of four intents (investigation, implementation, planning, research-QA) and delegates to exactly one downstream agent. Never edits and never implements. |
| `sks-code-investigation` | subagent | Answers "how does X work", "where is X", "what calls X" by fanning out read-only `sks-code-explore` children in parallel and synthesizing one evidence-cited explanation. |
| `sks-research` | subagent | Answers a question with cited internal and external evidence, delegating to `sks-code-explore` for repo facts and `sks-library-explore` or `websearch`/`webfetch` for outside facts. |
| `sks-planning` | subagent | Builds the task list and runs the adversarial `sks-negative-plan` loop for at most 5 rounds. Hands the finalized list to `sks-breakdown`; never writes the plan JSON. |
| `sks-negative-plan` | subagent | Adversarial plan critic. Returns severity-ordered holes in a draft plan (missing tasks, wrong ordering, unstated assumptions, weak verification, scope creep). Never approves and never edits. |
| `sks-breakdown` | subagent | Turns the finalized task list into execution waves and writes the immutable plan JSON to `.agents/plans/<slug>.json`. Writes only inside `.agents/plans/` and never delegates. |
| `sks-implementation` | subagent | Sole execution orchestrator and sole plan mutator. Triages scope, runs sequential waves of parallel `sks-coder` children, gates each with `sks-code-review` and `sks-goal-review`, invokes `sks-tie-breaker` past 10 cycles, and finalizes with `sks-git`. |
| `sks-coder` | subagent | Generic implementation agent. Executes exactly one plan task end to end with language-aware skills and a non-negotiable completion gate. |
| `sks-code-explore` | subagent | Read-only internal code discovery. Runs parallel grep, glob, and language-server search and returns absolute paths with evidence. Never edits and never delegates. |
| `sks-library-explore` | subagent | Read-only external docs and OSS discovery. Retrieves official documentation and real upstream usage, and cites every claim. |
| `sks-code-review` | subagent | Language-aware code review across five passes (correctness, style, security, compile, lint/format). Actually runs the affected tests and returns APPROVE or CHANGES_REQUESTED. |
| `sks-goal-review` | subagent | Goal-fidelity and behavior reviewer. Exercises the deliverable through its matching surface (API, visual, or logic), runs a regression check, and returns APPROVE or CHANGES_REQUESTED. |
| `sks-tie-breaker` | subagent | Review-loop tie breaker. Fires when a task exceeds 10 total review cycles or when code review and goal review disagree; rules CONTINUE or ACCEPT_AS_IS. |
| `sks-git` | subagent | Git finalizer. Stages only plan-listed files, makes one Conventional Commit per completed task, and pushes only when a remote exists. Never force-pushes and never delegates. |

## 2. Install

The definitions live in `agents/sks/`. Copy them into one of opencode's agent
directories.

Project scope (recommended when the pipeline should run only in this repo):

```bash
mkdir -p .opencode/agents
cp agents/sks/*.md .opencode/agents/
```

Global scope (the pipeline is available in every project):

```bash
mkdir -p ~/.config/opencode/agents
cp agents/sks/*.md ~/.config/opencode/agents/
```

`agents/sks/*.md` also matches `CONTRACTS.md` and `PLAN-SCHEMA.md`. Those two are
reference contracts, not agents, and they have no agent frontmatter. If you use the glob
above, delete or skip them, or scope the copy to `agents/sks/sks-*.md` so only real
definitions land in the agents directory.

### 2.1 Permissions need no translation

The definitions use the opencode v2 authoring shape: a frontmatter `permissions` array
of `{action, resource, effect}`. The opencode v2 runtime reads that same shape, and the
delegation action is `subagent`. There is no `task` key to render and no install-time
translation step.

| Authoring form | Runtime meaning |
|---|---|
| `action: subagent`, `resource: "sks-coder"`, `effect: allow` | permits launching the `sks-coder` child |
| `action: subagent`, `resource: "*"`, `effect: deny` | denies launching any child |
| `action: question`, `resource: "*"`, `effect: allow` | permits the `question` tool |
| `action: skill`, `resource: "*"`, `effect: allow` | permits the `skill` tool |

Copy the definitions as-is; the frontmatter is already runtime-valid. Dropping the
`subagent` rules denies delegation even when the depth config below is correct.

## 3. Runtime configuration

Nested subagents are off unless you raise `experimental.subagent_depth`. Add this to
`opencode.json` (project root or `~/.config/opencode/`):

```json
{
  "$schema": "https://opencode.ai/config.json",
  "experimental": {
    "subagent_depth": 4
  }
}
```

**Note.** In opencode v2 the key is **`experimental.subagent_depth`**, and the default
is `1`. (This differs from the v1.18.x top-level `subagent_depth` path.) The pipeline
needs it **>= 3**. The value `4` gives a maximum chain depth of three nested subagents
plus the root session, which is enough for
`sks-orchestrate -> sks-implementation -> sks-coder -> sks-code-explore`. A value of `3`
is the bare minimum; `4` leaves one level of slack.

The gate is `h < experimental.subagent_depth`, where `h` counts ancestors (the root
session is `h = 0`). A session may delegate only while its depth is below the limit.

## 4. Required grants

Each agent needs three grants to work as designed. The frontmatter action tokens are
the runtime keys; no translation is needed.

### 4.1 Delegation

The frontmatter writes `action: subagent`, which is the opencode v2 permission key. The
rule's resource is the target agent name (the value the `subagent` tool takes in its
`agent` parameter). Without it, no `sks-*` agent can hand work to another.

### 4.2 `question`

`question` is registered only when the client id is `app`, `cli`, or `desktop`, or when
`OPENCODE_ENABLE_QUESTION_TOOL` is set. A bare `opencode` process defaults to `cli`, so
interactive terminal use is fine. Headless or ACP runs are not. Subagents default to
`question: deny`, so the agents that ask the user (`sks-orchestrate`, `sks-planning`,
`sks-implementation`, `sks-tie-breaker`) must carry an explicit `question: allow`.

When `question` is unavailable or denied, those agents fall back to the `CONTRACTS.md`
section 6.3 rule: assume the recommended default, record it in the plan, and proceed.
They never hang.

### 4.3 `skill`

Every `sks-*` agent loads at least one skill (`ast-grep`, `git-master`, a language
skill, `playwright-cli`, or the `programming` fallback), so all 14 carry
`skill: allow`. On install, render it as the runtime key `skill`.

## 5. Failure mode when the config is not applied

If the snippet in section 3 is missing, or the grants in section 4 are not rendered, the
pipeline degrades silently. It does not print a clear setup error.

- **Depth stays at the default `1`.** The root session (`h = 0`) may still delegate, so
  `sks-orchestrate -> sks-implementation` looks fine. The child session sits at `h = 1`,
  and `1 < 1` is false, so it cannot delegate at all. Every deeper hop
  (`sks-implementation -> sks-coder`, `sks-implementation -> sks-planning`,
  `sks-orchestrate -> sks-implementation -> sks-coder -> sks-code-explore`) stops with
  the runtime error "Subagent depth limit reached (1). Increase
  \"experimental.subagent_depth\" to allow nested subagents." In practice the chain
  quietly never runs: the orchestrator reports a result, but the work below the first hop
  never happened.
- **Delegation is denied by default.** A child session is denied the `subagent` action
  unless its agent declares a `subagent` rule. An install that drops the rules leaves
  every `sks`-to-`sks` call rejected. The agents that would otherwise report a clear
  "permission denied" are the same ones that cannot start.

Both failures are silent at the point of the user request. That is why the depth value
and the permission rendering are installed together, not left to a runtime default.

## 6. Runtime contract (opencode v2)

Verified against the opencode v2 docs (https://opencode.ai/v2/docs/tools/#subagent) and
the v2 source (`packages/core/src/tool/plugin/subagent.ts`).

| Concern | opencode v2 |
|---|---|
| Delegation tool | **`subagent`** |
| Target parameter | **`agent`** (the target agent id) |
| Prompt parameter | `prompt` |
| Description parameter | `description` |
| Resume parameter | `sessionID` (optional; continue a prior child conversation) |
| Background parameter | `background: true` (optional; run the child in the background) |
| Permission action | **`subagent`**; resource = target agent id |

The delegation call is:

```text
subagent(agent="sks-...", description=..., prompt=..., sessionID=<id to resume>)
```

Add `background = true` to fan out. Never `task(agent=...)`, `task(target=...)`,
`subagent(subagent_type=...)`, or `subagent(target=...)`. The v1
`task` tool and its `subagent_type`/`task_id` parameters do not exist in opencode v2.

## 7. Plan file location

`agents/sks/PLAN-SCHEMA.md` owns the shape. Plans live at:

```text
.agents/plans/<slug>.json
```

`<slug>` is the kebab-case form of the goal (for example `add-user-auth`). One file per
plan, and the `slug` inside the JSON equals the filename stem. `sks-breakdown` writes
the file once; `sks-implementation` is its only runtime mutator. The directory is
gitignored runtime state.

## 8. Verify the install

After copying the definitions and merging the config, confirm the runtime sees the
depth and a delegating agent's rules:

```bash
opencode debug config
opencode debug agent sks-orchestrate
```

The first should print `"subagent_depth": 4` under the `experimental` section. The
second should show a `subagent` rule with `allow`, confirming the frontmatter permits
child launches.
