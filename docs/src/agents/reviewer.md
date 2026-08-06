# Reviewer Agent

**Agent name:** `git-automate-reviewer`  
**Prompt template:** `src/assets/prompts/reviewer.md`  
**Workflow stage:** Review Technical

## Role

The technical reviewer agent evaluates the implementation for code quality and correctness:

1. Reviews code changes for correctness and alignment with requirements.
2. Checks that code follows project conventions and is well-structured.
3. Verifies type safety — proper boundary parsing, exhaustive matching, no unsafe escapes (`any`, `unwrap`, etc.).
4. Confirms test coverage includes happy paths, edge cases, and error paths.
5. Reviews the PR description for clarity.

## Input

| Placeholder | Source |
|-------------|--------|
| `{{ISSUE_NUMBER}}` | Issue number |
| `{{PR_URL}}` | Pull request URL |
| `{{BRANCH_NAME}}` | Feature branch name |
| `{{ISSUE_TITLE}}` | Issue title |
| `{{PR_CHANGES}}` | PR diff / changes |

## Decisions

| Decision | Resulting status |
|----------|-----------------|
| Approve | → `Review Product` |
| Request changes | Stays in `Review Technical` |
| Escalate | → `Review Product` (with notes) |

## Expected output

A summary containing:
- The aspects reviewed
- Any issues found (code quality, type safety, test coverage)
- The decision: approve, request changes, or escalate
- The resulting issue status transition
