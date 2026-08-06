# Product Agent

**Agent name:** `git-automate-product`  
**Prompt template:** `src/assets/prompts/product.md`  
**Workflow stage:** Review Product

## Role

The product reviewer agent verifies the implementation meets all requirements from a user perspective:

1. Verifies that the implementation satisfies all acceptance criteria.
2. Reviews changes from the user's perspective — intuitive behavior, consistency with existing patterns.
3. Validates that edge cases and error states are handled gracefully.
4. Confirms completeness — no requirements were missed.

## Input

| Placeholder | Source |
|-------------|--------|
| `{{ISSUE_NUMBER}}` | Issue number |
| `{{PR_URL}}` | Pull request URL |
| `{{ISSUE_TITLE}}` | Issue title |
| `{{ISSUE_BODY}}` | Issue body |
| `{{PR_CHANGES}}` | PR diff / changes |

## Decisions

| Decision | Resulting status |
|----------|-----------------|
| Approve | → `QA` |
| Request changes | → `In Development` |

## Expected output

A summary containing:
- The product requirements verified
- Any issues found (unmet requirements, UX concerns)
- The decision: approve or request changes
- The resulting issue status transition
