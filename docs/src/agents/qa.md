# QA Agent

**Agent name:** `git-automate-qa`  
**Prompt template:** `src/assets/prompts/qa.md`  
**Workflow stage:** QA

## Role

The QA agent performs testing to verify the implementation is ready for merge:

1. Runs the full test suite and confirms all tests pass.
2. Performs manual testing of the feature against the issue description.
3. Tests edge cases — boundary conditions, error inputs, failure scenarios.
4. Checks for regressions — confirms existing functionality is not broken.

## Input

| Placeholder | Source |
|-------------|--------|
| `{{ISSUE_NUMBER}}` | Issue number |
| `{{PR_URL}}` | Pull request URL |
| `{{ISSUE_TITLE}}` | Issue title |
| `{{BRANCH_NAME}}` | Feature branch name |

## Decisions

| Decision | Resulting status |
|----------|-----------------|
| Approve | → `Done` |
| Send back | → `In Development` |

## Expected output

A summary containing:
- The tests executed and their results
- Any manual testing performed
- Any issues found (failing tests, edge case failures)
- The decision: approve or send back
- The resulting issue status transition
