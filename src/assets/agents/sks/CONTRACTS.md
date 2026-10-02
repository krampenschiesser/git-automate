# CONTRACTS.md

The shared delegation, permission, and routing contract for the 14 `sks-*` agents.
Read this before writing or changing any `agents/sks/sks-*.md` file. Every definition
in this directory must agree with every rule here. When a definition disagrees, the
definition is wrong, not this file.

Companion contracts:

- `agents/sks/PLAN-SCHEMA.md` owns the on-disk JSON plan shape.
- The opencode v2 docs own the runtime facts: https://opencode.ai/v2/docs/tools/#subagent
  and https://opencode.ai/v2/docs/agents. Section 7 pins the delegation contract.

## 0. The contract (repo authoring and installed runtime agree)

The `sks-*.md` definitions are written against **opencode v2**. There is one delegation
contract, and both layers use the same token.

| Layer | Where it lives | Delegation tool | Permission action | Rule shape |
|---|---|---|---|---|
| Repo authoring | `agent-schema.json`, `agents/sks/sks-*.md` | `subagent` | `subagent` | `permissions` array of `{action, resource, effect}` |
| Installed runtime (opencode v2) | resolved agent config | `subagent` | `subagent` | the same `permissions` array |

The runtime tool is **`subagent`**, and the permission action is also **`subagent`**,
whose resource is the target agent name (the value passed as the tool's `agent`
parameter). No translation step exists on install: the frontmatter action token
`subagent` is the runtime permission key, and the delegating agent's allow rule is
evaluated against the target agent id.

Two consequences that every author must hold in mind:

1. The v1 tool `task` and its parameters (`subagent_type`, `task_id`) do not exist in
   v2. A definition that still writes `task(...)` fails at call time. Section 1.1 is
   the verified form.
2. Every delegating agent must carry its own explicit `subagent: allow` rule. Without
   it, the runtime denies the child launch. Section 2 lists those rules; section 7.4
   covers the nested-depth setting.

## 1. Delegation envelope (used by every sks agent that delegates)

### 1.1 Verified invocation

The runtime tool is **`subagent`**. Its parameters are `agent` (the target agent),
`description` (a short 3-5 word label), `prompt` (the task for the agent), and two
optional parameters: `sessionID` (set only to continue a prior child session) and
`background` (set `true` to fan out without blocking). Use this form verbatim
everywhere:

```typescript
subagent(
  agent = "<target agent id, e.g. sks-coder>",
  description = "<short 3-5 word summary of the delegated work>",
  prompt = `<the full 6-section prompt, see 1.2>`
)
```

Resume a prior child session by adding `sessionID`. Nothing else changes:

```typescript
subagent(
  agent = "sks-coder",
  description = "<short summary>",
  prompt = "<continuation prompt with the new findings>",
  sessionID = "<prior child session id>"
)
```

Fan out independent children by adding `background = true`. The call returns
immediately and the runtime notifies the parent when each child finishes; do not poll
or duplicate the child's work while it runs (section 1.2, Parallel rule):

```typescript
subagent(
  agent = "<target agent id>",
  description = "<short summary>",
  prompt = `<the full 6-section prompt>`,
  background = true
)
```

Invalid forms. None of these are runtime calls, and none may appear as an invocation:

- `task(...)` in any form; the `task` tool does not exist in opencode v2
- `subagent(subagent_type="...", prompt="...")`; the target parameter is not
  `subagent_type`
- `subagent(target="...", prompt="...")`; the target parameter is not `target`

The only valid target parameter is `agent`. The old names (`task`, `subagent_type`,
`task_id`) are v1/prose-only and are never written as a call.

Before dispatch, run this checklist and fail the envelope if any answer is "no":

1. Is the tool literally `subagent`?
2. Is the target parameter literally `agent`?
3. Is the prompt parameter literally `prompt`?
4. Is `sessionID` present only when resuming, and absent on a fresh launch?
5. Does the prompt contain all 6 sections and at least 30 lines? (1.2)

### 1.2 The mandatory 6-section prompt

Every delegated prompt has exactly these six sections, in this order, with these
headings. A prompt missing any section is rejected.

```markdown
## 1. TASK
Quote the exact intent. Be obsessively specific about where, what, and why.
Name the single unit of work this agent owns. No bundled side quests.

## 2. EXPECTED OUTCOME
- [ ] Files created/modified: <exact absolute or repo-relative paths>
- [ ] Behavior: <exact observable result>
- [ ] Verification: `<exact command>` exits 0, plus the QA command to run

## 3. REQUIRED TOOLS
- <tool>: <what to read/search/run and why>
- <skill to load>: <when it applies>
- Language-server tools where present; state the grep/glob fallback explicitly.

## 4. MUST DO
- Follow the pattern in <reference file:lines>
- Produce the evidence the parent will read back
- Append findings to the notepad (append only, never overwrite)
- State assumptions explicitly when a question cannot be answered (section 6.3)

## 5. MUST NOT DO
- Do NOT touch files outside the named scope
- Do NOT add dependencies or expand scope
- Do NOT skip verification, and do NOT game tests
- Do NOT write the plan JSON unless this agent is the plan writer

## 6. CONTEXT
### Notepad Paths
- READ: .agents/notepads/sks-agent-pipeline/*.md
- WRITE: append to the appropriate category

### Inherited Wisdom
<conventions, gotchas, and decisions from the notepad>

### Dependencies
<what earlier tasks or agents produced that this work builds on>

### Open Decisions
<the recommended default when a question cannot be asked; see 6.3>
```

Length rule: **every prompt is at least 30 lines**. A prompt under 30 lines is too
short and must be expanded with concrete paths, commands, and inherited wisdom before
dispatch.

Parallel rule: independent delegations fire in one response, one `subagent` call each,
with `background = true` so the parent is not blocked while they run. Sequential only
when a named dependency (input value or shared file) truly blocks. Never poll a
background child or duplicate its work; the runtime notifies the parent on completion.

### 1.3 Parent synthesis rule

The end user never sees child output. The delegating agent reads the child result and
returns a synthesized answer with evidence. "The child said so" is never the answer.

## 2. Permission matrix (all 14 agents)

Rules are listed in evaluation order. The runtime evaluates rules in order and the
**last matching rule wins**, so a broad `deny` always precedes a narrow `allow`
(section 2.1). Every rule is `action: resource: effect`. Shell resource strings are
command patterns; the parenthetical in the Notes column lists the intended commands.

The rules are written as a frontmatter `permissions` array
`{action, resource, effect}`. The action `subagent` is the opencode v2 permission key,
whose resource is the target agent name (section 7).

| # | Agent | mode | Declared rules, in order (`action: resource: effect`) | Notes |
|---|---|---|---|---|
| 1 | `sks-code-explore` | subagent | `edit: *: deny`; `subagent: *: deny`; `skill: *: allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow`; `shell: "rg *": allow`; `shell: "git *": allow` | Read-only internal code discovery. Shell scoped to `rg` and read-only `git`. |
| 2 | `sks-library-explore` | subagent | `edit: *: deny`; `subagent: *: deny`; `skill: *: allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow`; `webfetch: *: allow`; `websearch: *: allow`; `shell: "gh *": allow`; `shell: "git *": allow` | Read-only external docs/OSS discovery. Shell scoped to `gh` and `git`. |
| 3 | `sks-coder` | subagent | `edit: *: allow`; `shell: *: allow`; `subagent: *: allow`; `skill: *: allow` | Generic implementation. May delegate to explores and load any language skill. |
| 4 | `sks-code-review` | subagent | `edit: *: deny`; `subagent: *: deny`; `skill: *: allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow`; `shell: *: allow` | Read-only. Shell for tests, linters, and builds. |
| 5 | `sks-goal-review` | subagent | `edit: *: deny`; `subagent: *: deny`; `skill: *: allow`; `shell: *: allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow` | Read-only. Shell for app/API/visual QA drivers. |
| 6 | `sks-negative-plan` | subagent | `edit: *: deny`; `subagent: *: allow`; `skill: *: allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow` | May delegate only to verify its own claims. |
| 7 | `sks-tie-breaker` | subagent | `edit: *: deny`; `subagent: *: allow`; `question: *: allow`; `skill: *: allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow` | May delegate for evidence; may ask the user a genuine tradeoff. |
| 8 | `sks-git` | subagent | `edit: *: deny`; `subagent: *: deny`; `shell: *: allow`; `skill: *: allow`; `read: *: allow` | Git finalizer. Loads the `git-master` skill. |
| 9 | `sks-code-investigation` | subagent | `edit: *: deny`; `subagent: *: allow`; `skill: *: allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow` | Coordinates parallel code exploration. |
| 10 | `sks-research` | subagent | `edit: *: deny`; `subagent: *: allow`; `skill: *: allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow`; `webfetch: *: allow`; `websearch: *: allow` | Answers with evidence from internal and external sources. |
| 11 | `sks-planning` | subagent | `edit: *: deny`; `subagent: *: allow`; `question: *: allow`; `skill: *: allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow` | Builds the task list, runs the negative-plan loop. Never writes the plan JSON. |
| 12 | `sks-breakdown` | subagent | `edit: *: deny`; `edit: ".agents/plans/**": allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow`; `skill: *: allow`; `subagent: *: deny` | Writes only `.agents/plans/**`. Never delegates. |
| 13 | `sks-implementation` | subagent | `edit: *: deny`; `edit: ".agents/plans/**": allow`; `subagent: *: allow`; `question: *: allow`; `shell: *: allow`; `skill: *: allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow` | Sole plan mutator. Shell for verification only. |
| 14 | `sks-orchestrate` | primary | `edit: *: deny`; `subagent: *: allow`; `question: *: allow`; `skill: *: allow`; `read: *: allow`; `glob: *: allow`; `grep: *: allow`; `webfetch: *: allow`; `websearch: *: allow` | Primary entry point. Never edits and never implements. |

Every one of the 14 agents appears exactly once. No agent has more than one `mode`.
Only `sks-orchestrate` is `primary`; the other 13 are `subagent`.

### 2.1 Rule ordering and least privilege

- Read-only agents deny `edit: *` and deny `subagent: *` (except where the agent must
  verify claims or gather evidence, in which case `subagent: *: allow` appears after
  the `edit` deny).
- Delegating agents allow `subagent: *`. Read-only agents that coordinate allow
  `subagent: *` but still deny `edit: *`.
- `question: *: allow` appears only on `sks-tie-breaker`, `sks-planning`,
  `sks-implementation`, and `sks-orchestrate`. Every other agent inherits the runtime
  subagent default (`question: deny`).
- `skill: *: allow` appears on every skill consumer. It is present on all 14 agents
  because every sks role loads at least one skill (`ast-grep`, `git-master`, a language
  skill, `playwright-cli`, or the `programming` fallback).
- `webfetch`/`websearch` appear only on research-capable agents:
  `sks-library-explore`, `sks-research`, and `sks-orchestrate`.
- Shell is allowed only where the role needs to run commands: the two explores (scoped
  to `rg`/`gh`/`git`), the two reviewers, `sks-git`, `sks-coder`, and
  `sks-implementation`. `sks-breakdown`, `sks-planning`, `sks-negative-plan`,
  `sks-tie-breaker`, `sks-code-investigation`, and `sks-research` declare no shell
  rule.
- `sks-breakdown` and `sks-implementation` narrow `edit` to `.agents/plans/**`, and the
  narrow allow is written **after** the broad `edit: *: deny`. Last match wins, so the
  plan path is writable and everything else stays denied. Reordering these two rules
  would silently lock the plan writer out.
- Do not add a rule that widens a role beyond its row. Least privilege is the default.

## 3. `sks-orchestrate` routing table

`sks-orchestrate` classifies every request into one of four intents and delegates to
exactly one downstream agent. It never implements directly.

| Intent | Trigger signals | Route to |
|---|---|---|
| Investigation | "how does X work", "where is X", "why does X", "explain", "trace", "what calls" | `sks-code-investigation` |
| Implementation | change verbs: "add", "fix", "implement", "refactor", "rename", "wire up", "build", "make it" | `sks-implementation` |
| Planning | "plan", "break down", "sequence the work", "design the approach", "what are the steps" | `sks-planning` |
| Research-QA | external sources: "best library for", "compare", "docs for", "is X supported", "what's recommended", "look it up" | `sks-research` |

`sks-implementation` then triages scope itself. A large or ambiguous request routes
through `sks-planning` -> `sks-breakdown` -> wave execution, still inside the
implementation intent.

### 3.1 Tie-break and default for ambiguous intent

When more than one signal fires, resolve in this order:

1. **A change verb wins.** If the request contains any change verb, route to
   `sks-implementation`, even when it also asks a question. The question is answered
   during triage.
2. **No change verb + a question** defaults to `sks-code-investigation`.
3. **No change verb + an explicit plan request** routes to `sks-planning`.
4. **No change verb + an external-source signal** routes to `sks-research`.

Conflict between planning and research: route to `sks-planning` when the deliverable is
a work plan, and to `sks-research` when the deliverable is an answer. If the intent is
still ambiguous after this order, apply the default:

- Question intent (no change verb): `sks-code-investigation`.
- Change intent (change verb present): `sks-implementation`.

State the chosen route and the signal that decided it in the first line of the
delegated prompt. When genuine product ambiguity remains, use `question` (section 6.3).

## 4. Loop thresholds and convergence

Two loops exist and each has one hard number. Never invent a third.

### 4.1 Negative-plan round cap: `<= 5`

`sks-planning` runs the adversarial negative-plan loop for at most 5 rounds. Round 5 is
the last round. There is no round 6.

Convergence at the cap:

1. `sks-planning` records every unresolved negative-plan finding in the plan's
   `openQuestions` array (append only), each with the recommended default that will be
   assumed.
2. `sks-planning` surfaces the unresolved items with the `question` tool. If `question`
   is unavailable or denied, apply the headless fallback in section 6.3: record the
   assumption and proceed.
3. `sks-planning` hands the finalized task list to `sks-breakdown` regardless of
   remaining findings. Unresolved items travel as recorded assumptions, not as a hang.
4. Never loop past 5. Never suppress a finding to escape the cap. Record it and move on.

### 4.2 Tie-breaker threshold: `> 10` cycles

`sks-tie-breaker` is invoked when either condition holds:

- the task's **total review cycles exceed 10** (`review.code.cycles + review.goal.cycles
  > 10`), across all restarts, or
- `sks-code-review` and `sks-goal-review` return **conflicting verdicts** on the same
  task.

The threshold is strictly greater than 10. Ten cycles do not trigger it; eleven do.
Cycle counts persist across restarts and are never reset (PLAN-SCHEMA.md sections 11
and 10). The tie-breaker returns `CONTINUE` (naming the single remaining blocker) or
`ACCEPT_AS_IS` (with the residual risk), and `sks-implementation` records that decision
in `task.tieBreaker`.

## 5. Resume-validity procedure

On resume, `sks-implementation` never trusts a stored session id. It probes each one.

1. Load `.agents/plans/<slug>.json`.
2. For each task with a stored `coder.sessionId`, and any stored `review.code.sessionId`
   or `review.goal.sessionId`, probe its resumability: resolve the id in the runtime
   session registry or attempt a minimal continuation with `sessionID = <id>`.
3. **Resumable:** keep the id and continue that same session with `sessionID`. A
   continuation keeps the agent's full context; do not restart it.
4. **Un-resumable:** set the stored id to `null`, then restart that task fresh
   **without changing its direction**. Direction means `title`, `description`,
   `context`, `dependencies`, `filesTouched`, `successCriteria`, and `verification`.
   Restarting never edits direction. Increment `coder.attempts`, append a restart note
   to `task.notes` (append only), and launch a new session for the same task.
5. Never reset `coder.attempts` or `review.*.cycles` on resume. A restart is another
   attempt on the same counters, not a reset.
6. Changing direction is a different operation. It is never silent. It requires
   `question`, or the headless fallback in section 6.3.

## 6. Failure and edge behavior

### 6.1 Session resume over restart

Prefer resuming the same child session over a fresh one. A resumed session keeps the
context that produced the current state, so a fix prompt lands on an informed agent.
Start fresh only when resume-validity fails (section 5).

### 6.2 No re-entry

`sks-planning` must never call `sks-implementation`, and no sks agent may delegate to
itself. The pipeline is acyclic. The two allowed deep chains are
`sks-orchestrate -> sks-implementation -> sks-coder -> sks-code-explore` and
`sks-orchestrate -> sks-implementation -> sks-planning -> sks-breakdown`.

### 6.3 Headless fallback for the `question` tool

`question` is registered only when the client id is `app`, `cli`, or `desktop`, or when
`OPENCODE_ENABLE_QUESTION_TOOL` is set. Subagents default to `question: deny`. So a run
can reach a decision point with no way to ask the user. When that happens:

1. Do **not** hang, block, retry forever, or fail the run.
2. Adopt the recommended default for the decision, as stated by the asking agent.
3. Record the assumption in the plan JSON: append the item to `openQuestions` (append
   only), and when it is task-scoped append it to `task.notes` as well.
4. Proceed with the assumed default. If it is later contradicted, that is a new
   decision, not a reason the run was invalid.

The same rule applies whenever `question` is denied by permission even on an
interactive client. "Cannot ask" always means "assume, record, proceed".

## 7. Runtime contract (opencode v2)

This section pins the delegation facts. They are verified against the opencode v2 docs
(https://opencode.ai/v2/docs/tools/#subagent) and the v2 source
(`packages/core/src/tool/plugin/subagent.ts`). Every definition, this contract, and the
README must agree with it. A mismatch is a REJECT.

### 7.1 Runtime tool parameters (the authoritative set)

- `agent`: required; the target agent id (must be a `subagent`- or `all`-mode agent).
- `description`: required; a short 3-5 word label.
- `prompt`: required; the task for the agent.
- `model`: optional; never set unless the user explicitly asks for a model or variant.
- `sessionID`: optional; set only to continue a prior child conversation.
- `background`: optional boolean; `true` runs the child in the background and returns
  immediately.

### 7.2 Permission action

Delegation is gated by the permission action **`subagent`**, whose resource is the
target agent id (the tool's `agent` value). The frontmatter `permissions` array already
uses that action, so no install-time translation is required. Only agents whose `mode`
is `subagent` (or `all`) can be launched as a child; a `primary` agent is rejected.

### 7.3 Delegation call

`subagent(agent="sks-...", description=..., prompt=..., sessionID=<id to resume>)`.
Add `background = true` to fan out. Never `task(...)`,
`subagent(subagent_type=...)`, or `subagent(target=...)` (section 1.1).

### 7.4 Nested delegation and depth

Every delegating `sks-*` agent declares its own explicit `subagent: allow` rule. Without
it, the runtime denies the child launch; section 2 lists those rules.

Config raises `experimental.subagent_depth` (default `1`) so nested children can
delegate; the README documents the exact key. Deep chains silently stop running if it is
not raised.

## 8. Authoring checklist

Before any `sks-*.md` definition is accepted, confirm all of these:

- [ ] The agent appears exactly once in the section 2 matrix with the matching `mode`.
- [ ] Its declared rules match its matrix row, in order, with the broad deny before any
      narrow allow.
- [ ] Its delegation calls use the section 1.1 verified form
      (`subagent(agent=, description=, prompt=)`), never a prose alias.
- [ ] No `task(...)`, `subagent(subagent_type=...)`, or
      `subagent(target=...)` appears as a call.
- [ ] When it asks the user, it carries `question: *: allow` and the section 6.3
      fallback.
- [ ] Its prompts carry all 6 sections and reach 30 lines.
- [ ] Its route, if it is `sks-orchestrate`, comes from the section 3 table plus the
      section 3.1 tie-break.
- [ ] It agrees with section 7's runtime contract. A mismatch is rejected.
