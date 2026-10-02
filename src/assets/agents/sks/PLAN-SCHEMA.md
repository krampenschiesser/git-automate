# PLAN-SCHEMA.md

The JSON plan document contract for the `sks-*` agent pipeline. This file is the
single source of truth for how a plan is shaped on disk. Three roles consume it:

- `sks-breakdown` writes the plan once.
- `sks-implementation` is the sole mutator of runtime fields.
- `sks-code-review` and `sks-goal-review` read task success criteria and write
  nothing.

If an agent is about to write the plan and its action is not listed in the
mutability table below, it is wrong. Stop and re-read this file.

## 1. Storage location

Plans live at:

```
.agents/plans/<slug>.json
```

`<slug>` is the kebab-case form of the goal (lowercase, alphanumeric words joined by
single hyphens, for example `add-user-auth`). One file per plan. `<slug>` inside the
JSON must equal the filename stem. `.agents/plans/` is gitignored runtime state; the
agent definitions that read this contract are not.

## 2. Document example

The block below is a complete, valid plan. It parses cleanly. Copy its shape.

```json
{
  "schemaVersion": 1,
  "slug": "add-user-auth",
  "goal": "Add JWT-based user authentication to the API",
  "projectRoot": "/home/user/project",
  "language": "typescript",
  "status": "pending",
  "createdAt": "2026-10-02T12:00:00Z",
  "updatedAt": "2026-10-02T12:00:00Z",
  "openQuestions": [],
  "sessions": [],
  "waves": [
    {
      "id": "wave-1",
      "status": "pending",
      "tasks": [
        {
          "id": "task-1",
          "title": "Add password hashing helper",
          "type": "implementation",
          "description": "Create a password hashing module using argon2 and wire it into the user model. The module exports hashPassword and verifyPassword.",
          "context": {
            "files": ["src/auth/hash.ts", "src/models/user.ts"],
            "patterns": ["src/config/loader.ts"],
            "references": ["https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html"],
            "notes": "Reuse the existing config loader for the argon2 cost parameters."
          },
          "dependencies": [],
          "filesTouched": ["src/auth/hash.ts", "src/models/user.ts"],
          "successCriteria": {
            "code": "hashPassword and verifyPassword are exported; the unit test passes; npx tsc --noEmit exits 0.",
            "logic": "A stored hash verifies the correct password and rejects a wrong one."
          },
          "verification": {
            "commands": ["npm test -- auth/hash", "npx tsc --noEmit"],
            "qa": [
              {
                "description": "Run the hash round-trip smoke check",
                "command": "node scripts/smoke-hash.mjs"
              }
            ]
          },
          "status": "pending",
          "coder": {
            "agent": "sks-coder",
            "sessionId": null,
            "attempts": 0
          },
          "review": {
            "code": {
              "verdict": null,
              "cycles": 0,
              "sessionId": null
            },
            "goal": {
              "verdict": null,
              "cycles": 0,
              "sessionId": null
            }
          },
          "tieBreaker": null,
          "commit": null,
          "notes": []
        }
      ]
    }
  ]
}
```

## 3. Field reference

### 3.1 Top level

| Field | JSON type | Required | Notes |
|---|---|---|---|
| `schemaVersion` | integer | yes | Version of this contract. Current value: `1`. Immutable after write. |
| `slug` | string | yes | Kebab-case of `goal`. Must equal the filename stem. Immutable. |
| `goal` | string | yes | One-sentence statement of the finished outcome. Immutable. |
| `projectRoot` | string | yes | Absolute path to the target project root. Immutable. |
| `language` | string | yes | Primary language id (for example `typescript`, `rust`, `go`). Immutable. |
| `status` | string | yes | Plan status. Value from the status enum. Mutable. |
| `createdAt` | string | yes | ISO 8601 UTC timestamp set by `sks-breakdown`. Immutable. |
| `updatedAt` | string | yes | ISO 8601 UTC timestamp, refreshed on every atomic update. Mutable. |
| `openQuestions` | array of string | yes | Open questions and recorded assumptions. Append only. Empty array when none. |
| `sessions` | array of session record | yes | Registry of child sessions used so far. Empty array on a fresh plan. Mutable. |
| `waves` | array of wave | yes | Ordered list of execution waves. Order is execution order. Immutable after write. |

### 3.2 Wave object

| Field | JSON type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Stable id, `wave-<n>`. Immutable. |
| `status` | string | yes | Wave status. Value from the status enum. Mutable. |
| `tasks` | array of task | yes | Tasks in this wave. Order within a wave is not significant. Mutable element status only. |

### 3.3 Task object

| Field | JSON type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Stable id, `task-<n>`. Unique across the plan. Immutable. |
| `title` | string | yes | Short imperative title. Immutable. |
| `type` | string | yes | Task kind. Value from the type enum. Immutable. |
| `description` | string | yes | What to build and how. Enough for a coder with no other context. Immutable. |
| `context` | object | yes | See 3.4. Immutable. |
| `dependencies` | array of string | yes | Task ids that must complete first. Empty array when none. Immutable. |
| `filesTouched` | array of string | yes | Paths the task is expected to change, used for parallel-safety and staging. Immutable. |
| `successCriteria` | object | yes | See 3.5. Immutable. |
| `verification` | object | yes | See 3.6. Immutable. |
| `status` | string | yes | Task status. Value from the status enum. Mutable. |
| `coder` | object | yes | See 3.7. Mutable. |
| `review` | object | yes | See 3.8. Mutable. |
| `tieBreaker` | object or null | yes | `null` until `sks-tie-breaker` rules. See 3.9. Mutable. |
| `commit` | string or null | yes | `null` until `sks-git` commits. Then the commit SHA. Mutable. |
| `notes` | array of string | optional | Optional extension. Runtime status notes (restart notes, blockers). Append only. Defaults to `[]`. |

### 3.4 Task `context`

| Field | JSON type | Required | Notes |
|---|---|---|---|
| `files` | array of string | yes | Files to read first. Paths relative to `projectRoot`. |
| `patterns` | array of string | yes | Existing files to model the change on. Empty array when none. |
| `references` | array of string | yes | URLs or doc paths backing the approach. Empty array when none. |
| `notes` | string | yes | Free-text guidance for the coder. Empty string when none. |

### 3.5 Task `successCriteria`

| Field | JSON type | Required | Notes |
|---|---|---|---|
| `code` | string | yes | Machine-checkable outcome: tests, compile, lint, exact commands. Code review reads this. |
| `logic` | string | yes | Behavioral outcome the change must produce. Goal review reads this. |

### 3.6 Task `verification`

| Field | JSON type | Required | Notes |
|---|---|---|---|
| `commands` | array of string | yes | Commands that must exit 0. Empty array allowed only for a pure manual-verification task. |
| `qa` | array of qa item | yes | Agent-executable checks, see 3.6.1. Empty array allowed only when `commands` is non-empty. |

#### 3.6.1 qa item

| Field | JSON type | Required | Notes |
|---|---|---|---|
| `description` | string | yes | What the check proves. |
| `command` | string | yes | The exact command an agent runs. A human would run this same command. |

### 3.7 Task `coder`

| Field | JSON type | Required | Notes |
|---|---|---|---|
| `agent` | string | yes | Coder agent id, normally `sks-coder`. |
| `sessionId` | string or null | yes | Stored child session id for resume. `null` before first launch or after an un-resumable restart. |
| `attempts` | integer | yes | Count of coder attempts. Starts at 0. Persists across restarts, never resets on resume. |

### 3.8 Task `review`

`review` holds two reviewer slots, `code` and `goal`. Each slot has the same shape.

| Field | JSON type | Required | Notes |
|---|---|---|---|
| `verdict` | string or null | yes | `null` until review runs, then a verdict enum value. |
| `cycles` | integer | yes | Completed coder/reviewer cycles for this slot. Starts at 0. Persists across restarts, never resets on resume. |
| `sessionId` | string or null | yes | Stored reviewer session id, or `null`. |

### 3.9 Task `tieBreaker`

`null` when never invoked. When invoked, an object:

| Field | JSON type | Required | Notes |
|---|---|---|---|
| `decision` | string | yes | `CONTINUE` or `ACCEPT_AS_IS`. |
| `reason` | string | yes | The single remaining blocker (`CONTINUE`) or the residual risk accepted (`ACCEPT_AS_IS`). |
| `at` | string | yes | ISO 8601 UTC timestamp. |

### 3.10 Session record (top-level `sessions[]`)

| Field | JSON type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | Child session id. |
| `agent` | string | yes | Agent id that owns the session. |
| `taskId` | string or null | yes | Task id when the session is task-scoped, else `null`. |
| `kind` | string | yes | `coder`, `code-review`, `goal-review`, `tie-breaker`, or `planning`. |
| `startedAt` | string | yes | ISO 8601 UTC timestamp. |
| `status` | string | yes | Value from the status enum. |

## 4. Enums

### 4.1 `status`

Used by top-level `status`, wave `status`, task `status`, and session record `status`.

| Value | Meaning |
|---|---|
| `pending` | Not started. The initial value for every wave and task. |
| `in-progress` | A coder or reviewer is actively working. |
| `completed` | Success criteria met and both reviewers approved (or tie-breaker accepted). |
| `blocked` | Cannot proceed. A reason must be recorded in `notes`. |
| `skipped` | Deliberately not executed. A reason must be recorded in `notes`. |

### 4.2 `type` (task kind)

| Value | Meaning |
|---|---|
| `implementation` | Produces product/source code changes. |
| `unit-test` | Adds or modifies unit tests. |
| `e2e-test` | Adds or modifies end-to-end tests. |
| `manual-verification` | An agent-executable smoke or command check. See section 8. |
| `benchmark` | A performance measurement. See section 9. |

### 4.3 `verdict` (review)

| Value | Meaning |
|---|---|
| `null` | Review has not run. |
| `APPROVE` | The reviewed dimension passes. |
| `CHANGES_REQUESTED` | Findings must be fixed before the task can advance. |

## 5. Ownership and the single-writer rule

One plan file, one writer at a time.

- `sks-breakdown` writes the plan **once**. That write defines the immutable spec:
  goal, slug, project root, waves, task ids, titles, types, descriptions, context,
  dependencies, files, success criteria, and verification. It sets every wave and task
  `status` to `pending`. After this write the spec is frozen for the life of the plan;
  the only way it changes is a new plan at a new `schemaVersion`.
- `sks-implementation` is the **sole mutator** after that. It owns every runtime
  update to status, session, and cycle fields, and it names the `commit` after
  `sks-git` reports one.
- Coders, reviewers, the tie-breaker, git, and workers **report results only**. They
  never write the plan. A coder that wants a field changed says so in its report; the
  sole writer applies it.

Because only one process (the `sks-implementation` session) writes, parallel coders
cannot race on the file. Coders run concurrently; their results are applied serially
by the single writer.

## 6. Atomic update rule

Every mutation is atomic. The sole writer never edits the file in place.

1. Load `.agents/plans/<slug>.json` into memory.
2. Apply the change to the in-memory object.
3. Write the full object to a temporary file in the same directory, for example
   `.agents/plans/<slug>.json.tmp`.
4. Flush and close it.
5. `rename()` the temporary file over the target. On the same filesystem this replace
   is atomic: a reader sees either the old file or the new one, never a partial write.

A crash mid-write leaves the temp file behind and the real plan untouched. Delete the
stale temp on next load. Do not hold two plan files open for write at once.

## 7. Mutability rule

`sks-implementation` may change only the fields in the left column. Everything in the
right column is immutable spec and must never change after `sks-breakdown` writes it.

| Mutable (runtime state) | Immutable (spec) |
|---|---|
| top-level `status` | `schemaVersion`, `slug`, `goal`, `projectRoot`, `language`, `createdAt` |
| top-level `updatedAt` | top-level `waves` order and membership |
| top-level `sessions` | wave `id` and task order |
| top-level `openQuestions` (append only) | task `id`, `title`, `type`, `description` |
| wave `status` | task `context`, `dependencies`, `filesTouched` |
| task `status` | task `successCriteria`, `verification` |
| task `coder.agent`, `coder.sessionId`, `coder.attempts` | |
| task `review.code.*`, `review.goal.*` | |
| task `tieBreaker` | |
| task `commit` | |
| task `notes` (append only, optional extension) | |

Changing a task direction (its goal, description, context, dependencies, files, or
success criteria) is not a mutation and is never done silently. If direction must
change, use the `question` tool. If `question` is unavailable or denied (headless), do
not change direction: adopt the recommended default, append the assumption to
`openQuestions`, and proceed.

## 8. `manual-verification` is agent-executable

`manual-verification` means an agent-executable smoke test or command check. It is a
command or script that an agent runs to prove behavior, and a human would run that
same command. Its evidence is the command plus its captured output.

It is never a human-only step. There is no "verify by hand and tick a box" task, no
"ask a person to click around", and no acceptance criterion that only a human can
satisfy. If a check cannot be expressed as a command or script an agent can run, it is
not a `manual-verification` task. Give every `manual-verification` task a concrete
`verification.qa[].command` and record the output as evidence.

## 9. `benchmark` scope

Use `benchmark` only when a task changes performance-sensitive behavior: hot paths,
data-structure choice, query shape, allocation, concurrency, or an explicit perf
budget. A benchmark task states the baseline, the metric, and the command that
produces the number, and its result must be reproducible from that command.

Do not add `benchmark` tasks for work that has no plausible performance effect. A
missing benchmark on non-perf work is correct, not a gap.

## 10. Resume-validity rule

On resume, `sks-implementation` loads the plan and, for each task with a stored
`coder.sessionId` (and any stored reviewer `sessionId`), probes whether the session
can be resumed.

- If it can be resumed, resume it and continue.
- If it cannot be resumed, set the stored session id to `null` and restart that task
  fresh, **without changing the task direction**, and record the restart as a status
  note in `task.notes`.

A restart never edits `title`, `description`, `context`, `dependencies`,
`filesTouched`, `successCriteria`, or `verification`. It only clears the dead session
id, increments `coder.attempts`, appends the note, and launches a new session for the
same task.

## 11. Cycle-count persistence

`review.code.cycles`, `review.goal.cycles`, and `coder.attempts` persist across
restarts and are **never reset on resume**. A restart is another attempt, so
`coder.attempts` goes up, never back to zero. Cycle counts are cumulative for the life
of the task; they drive the tie-breaker threshold (invoke `sks-tie-breaker` when a
task exceeds 10 total cycles, or when the two reviewers return conflicting verdicts).

Never zero these counters to "clear" a task. If a task is genuinely restarted from
scratch, that restart is a new attempt on the same counters, not a reset.

## 12. Commit strategy

`sks-git` commits after all waves pass. It stages only `filesTouched` paths from
completed tasks, never unrelated in-progress work. One Conventional Commit per task,
grouped by task id. On commit it reports the SHA, and the sole writer records that SHA
in the task's `commit` field. `commit` stays `null` until then.

## 13. Optional extensions

Only one extension exists: task `notes` (array of string, default `[]`). It is
runtime-only, append-only, and must never carry spec content. Do not add other fields.
If a consumer needs more data, it belongs in `openQuestions` or a new `schemaVersion`.
