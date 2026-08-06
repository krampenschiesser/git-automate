# Workflow Engine

The workflow engine orchestrates the issue lifecycle through five sequential checks.

## Workflow steps

```rust
pub enum WorkflowStep {
    Setup,          // Create project, ensure status options + sessionId field
    OpencodeCheck,  // Verify OpenCode server health + required agents
    Triage,         // Find @ai issues, add to project, start triage session
    Todo,           // Find Todo items, create branches, start developer session
    Review,         // Find review-state items, fill prompts, start review session
}
```

Each step is executed in order by `Workflow::run_all()`. Errors are caught and logged — the method always returns `Ok(())`.

## Step details

### 1. Setup (`run_setup_check`)

For each project:
1. Parse the repository URL to extract `owner` and `repo`.
2. If `projectId` is missing, create a new Project V2 on GitHub and write the ID back to the config file.
3. Ensure the project has all seven workflow status options.
4. Ensure the project has a `sessionId` text field.

### 2. OpenCode check (`run_opencode_check`)

For each project with an OpenCode config:
1. Call `GET /global/health` on the OpenCode server.
2. If healthy, call `GET /agent` to verify all six required agents are installed.
3. Log warnings for missing agents or an unhealthy server.

### 3. Triage (`run_triage_check`)

For each project with an OpenCode config:
1. Resolve the project context (owner, repo, project ID, field IDs).
2. Fetch all open issues from the repository via REST.
3. Filter issues matching `titlePattern` (default: `@ai.*`).
4. For each matching issue:
   - If not already in the project, add it via `addProjectV2ItemById`.
   - Set status to `Triage`.
   - If no `sessionId` exists, start a `git-automate-triage` session and store the ID.

### 4. Todo (`run_todo_check`)

For each project with an OpenCode config:
1. Resolve field IDs (Status, sessionId).
2. Fetch all project items and the issue body map.
3. For each item with `Status == Todo` and no session:
   - Clone the repo to `/tmp/git-automate-work/{owner}-{repo}` (if not already cloned).
   - Create branch `issue-{N}` from the default branch (if it doesn't exist).
   - Start a `git-automate-developer` session and store the session ID.

### 5. Review (`run_review_check`)

For each project with an OpenCode config:
1. Resolve field IDs.
2. For each review state (`Review Technical`, `Review Product`, `QA`):
   - Find items with that status and no session.
   - Load the corresponding prompt template.
   - Fill placeholders: `{{ISSUE_NUMBER}}`, `{{BRANCH_NAME}}`, `{{ISSUE_TITLE}}`, `{{ISSUE_BODY}}`, `{{PR_URL}}`, `{{PR_CHANGES}}`.
   - Start the appropriate agent session (`git-automate-reviewer`, `git-automate-product`, or `git-automate-qa`).
   - Store the session ID.

## Error handling

Every check wraps per-project work in an async block that catches errors:

```rust
for (project_name, project_config) in &self.deps.config.projects {
    if let Err(e) = some_check(&deps, &ctx, &oc).await {
        tracing::error!("Check failed for {}: {}", project_name, e);
    }
}
```

This means a failure in one project never blocks processing of other projects.

## Concurrency control

When `concurrency` is set in the config, `start_opencode_session()` queries the active session count before creating a new session. If the count meets or exceeds the limit, the session is skipped with a warning.
