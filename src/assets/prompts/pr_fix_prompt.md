{{! PR Fix Prompt Template — external markdown template rendered by handlebars in build_pr_fix_prompt }}
# PR Feedback — {{owner}}/{{repo}} #{{pr_number}}

## Unresolved File+Line Comments
(Inline review comments on specific diff lines. Each has a thread ID for resolution after fixing.)
{{#if unresolved_review_comments}}
{{#each unresolved_review_comments}}
Thread ID: {{thread_id}}
File: {{path}}
Line: {{line_display}}
Diff Side: {{diff_side}}
Author: {{author}}
Created: {{created_at}}
Comment: {{body}}

{{/each}}
{{else}}
No unresolved file+line comments.

{{/if}}
## General Comments
(Non-inline comments on the PR as a whole.)
{{#if normal_comments}}
{{#each normal_comments}}
Author: {{author}}
Created: {{created_at}}
Comment: {{body}}

{{/each}}
{{else}}
No general comments.

{{/if}}
{{#if resolved_count}}
## Excluded: {{resolved_count}} resolved comment(s)

{{/if}}
## Instructions
Address all unresolved file+line comments above. When a fix is applied, resolve the corresponding review thread using its Thread ID.