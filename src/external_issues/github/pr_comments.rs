//! PR comment service: reads review threads and normal comments from a
//! pull request, builds an LLM-friendly fix prompt, and resolves threads.

use crate::external_issues::github::client::{GitHubClient, GitHubError};
use crate::external_issues::github::types::*;
use serde::Serialize;

pub use crate::external_issues::github::types::{PRCommentInfo, PRComments, ReviewCommentInfo};

impl ReviewThreadNode {
    pub fn to_review_comments(&self) -> Vec<ReviewCommentInfo> {
        self.comments
            .nodes
            .iter()
            .map(|comment| ReviewCommentInfo {
                thread_id: self.id.clone(),
                comment_id: comment.id.clone(),
                path: self.path.clone(),
                line: self.line,
                original_line: self.original_line,
                diff_side: self.diff_side.clone(),
                is_resolved: self.is_resolved,
                body: comment.body.clone(),
                author: comment
                    .author
                    .as_ref()
                    .map(|a| a.login.clone())
                    .unwrap_or_default(),
                created_at: comment.created_at.clone(),
            })
            .collect()
    }
}

impl IssueCommentNode {
    pub fn to_pr_comment_info(&self) -> PRCommentInfo {
        PRCommentInfo {
            id: self.id.clone(),
            body: self.body.clone(),
            author: self
                .author
                .as_ref()
                .map(|a| a.login.clone())
                .unwrap_or_default(),
            created_at: self.created_at.clone(),
        }
    }
}

// `line_display` is pre-computed because Handlebars has no `unwrap_or_else`
// helper — the fallback chain (`rc.line` → `rc.original_line` → `0`) must
// be resolved before rendering.
#[derive(Serialize)]
struct PrReviewCommentEntry {
    thread_id: String,
    path: String,
    line_display: i64,
    diff_side: String,
    author: String,
    created_at: String,
    body: String,
}

#[derive(Serialize)]
struct PrFixPromptData {
    owner: String,
    repo: String,
    pr_number: i64,
    unresolved_review_comments: Vec<PrReviewCommentEntry>,
    resolved_count: usize,
    normal_comments: Vec<PRCommentInfo>,
}

pub fn build_pr_fix_prompt(
    owner: &str,
    repo: &str,
    pr_number: i64,
    comments: &PRComments,
) -> String {
    let unresolved: Vec<&ReviewCommentInfo> = comments
        .review_comments
        .iter()
        .filter(|c| !c.is_resolved)
        .collect();
    let resolved_count = comments.review_comments.len() - unresolved.len();

    let data = PrFixPromptData {
        owner: owner.to_string(),
        repo: repo.to_string(),
        pr_number,
        unresolved_review_comments: unresolved
            .iter()
            .map(|rc| PrReviewCommentEntry {
                thread_id: rc.thread_id.clone(),
                path: rc.path.clone(),
                line_display: rc.line.unwrap_or_else(|| rc.original_line.unwrap_or(0)),
                diff_side: rc.diff_side.clone(),
                author: rc.author.clone(),
                created_at: rc.created_at.clone(),
                body: rc.body.clone(),
            })
            .collect(),
        resolved_count,
        normal_comments: comments.normal_comments.clone(),
    };

    let template = include_str!("../../assets/prompts/pr_fix_prompt.md");
    let hbs = handlebars::Handlebars::new();
    hbs.render_template(template, &data)
        .expect("pr_fix_prompt template should render successfully")
}

pub struct PRCommentService<'a> {
    client: &'a GitHubClient,
}

impl<'a> PRCommentService<'a> {
    pub fn new(client: &'a GitHubClient) -> Self {
        Self { client }
    }

    pub async fn fetch_all_comments(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
    ) -> Result<PRComments, GitHubError> {
        let review_threads = self
            .client
            .list_pr_review_comments(owner, repo, pr_number)
            .await?;
        let normal_comments = self.client.list_pr_comments(owner, repo, pr_number).await?;

        let review_comments: Vec<ReviewCommentInfo> = review_threads
            .iter()
            .flat_map(ReviewThreadNode::to_review_comments)
            .collect();

        let normal_comments: Vec<PRCommentInfo> = normal_comments
            .iter()
            .map(IssueCommentNode::to_pr_comment_info)
            .collect();

        Ok(PRComments {
            review_comments,
            normal_comments,
        })
    }

    pub async fn build_fix_prompt(
        &self,
        owner: &str,
        repo: &str,
        pr_number: i64,
    ) -> Result<String, GitHubError> {
        let comments = self.fetch_all_comments(owner, repo, pr_number).await?;
        Ok(build_pr_fix_prompt(owner, repo, pr_number, &comments))
    }

    pub async fn resolve_thread(&self, thread_id: &str) -> Result<(), GitHubError> {
        self.client.resolve_review_thread(thread_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review_comment(
        thread_id: &str,
        path: &str,
        line: Option<i64>,
        is_resolved: bool,
        body: &str,
        author: &str,
    ) -> ReviewCommentInfo {
        ReviewCommentInfo {
            thread_id: thread_id.to_string(),
            comment_id: format!("{}-comment", thread_id),
            path: path.to_string(),
            line,
            original_line: line,
            diff_side: "RIGHT".to_string(),
            is_resolved,
            body: body.to_string(),
            author: author.to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    fn normal_comment(body: &str, author: &str) -> PRCommentInfo {
        PRCommentInfo {
            id: "comment-1".to_string(),
            body: body.to_string(),
            author: author.to_string(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    // T34: prompt includes unresolved review comments with file:line metadata
    #[test]
    fn build_pr_fix_prompt_includes_review_comments() {
        let comments = PRComments {
            review_comments: vec![
                review_comment(
                    "thread-1",
                    "src/main.rs",
                    Some(42),
                    false,
                    "Use proper error handling here",
                    "reviewer",
                ),
                review_comment(
                    "thread-2",
                    "src/utils.rs",
                    Some(10),
                    true,
                    "Already fixed",
                    "reviewer",
                ),
            ],
            normal_comments: vec![normal_comment(
                "Consider adding tests for this",
                "reviewer2",
            )],
        };

        let prompt = build_pr_fix_prompt("octocat", "hello-world", 42, &comments);
        println!("=== PROMPT OUTPUT ===\n{}", prompt);

        assert!(prompt.contains("src/main.rs"));
        assert!(prompt.contains("Line: 42"));
        assert!(prompt.contains("Use proper error handling here"));
        assert!(prompt.contains("Thread ID: thread-1"));
        assert!(!prompt.contains("Already fixed"));
        assert!(prompt.contains("1 resolved comment"));
    }

    // T35: prompt excludes resolved comments
    #[test]
    fn build_pr_fix_prompt_excludes_resolved_comments() {
        let comments = PRComments {
            review_comments: vec![
                review_comment(
                    "thread-1",
                    "src/main.rs",
                    Some(42),
                    true,
                    "Already fixed",
                    "reviewer",
                ),
                review_comment(
                    "thread-2",
                    "src/lib.rs",
                    Some(10),
                    false,
                    "Needs attention",
                    "reviewer",
                ),
            ],
            normal_comments: vec![],
        };

        let prompt = build_pr_fix_prompt("owner", "repo", 1, &comments);

        assert!(prompt.contains("src/lib.rs"));
        assert!(prompt.contains("Needs attention"));
        assert!(!prompt.contains("Already fixed"));
        assert!(prompt.contains("1 resolved comment"));
    }

    // T36: prompt includes normal comments
    #[test]
    fn build_pr_fix_prompt_includes_normal_comments() {
        let comments = PRComments {
            review_comments: vec![],
            normal_comments: vec![normal_comment("Nice PR!", "reviewer1")],
        };

        let prompt = build_pr_fix_prompt("owner", "repo", 1, &comments);

        assert!(prompt.contains("Nice PR!"));
        assert!(prompt.contains("General Comments"));
    }

    // T37: prompt fallback when no comments
    #[test]
    fn build_pr_fix_prompt_empty_fallback() {
        let comments = PRComments::default();

        let prompt = build_pr_fix_prompt("owner", "repo", 1, &comments);

        assert!(prompt.contains("No unresolved file+line comments"));
        assert!(prompt.contains("No general comments"));
    }

    // T38: prompt includes PR reference
    #[test]
    fn build_pr_fix_prompt_includes_pr_reference() {
        let comments = PRComments::default();

        let prompt = build_pr_fix_prompt("octocat", "hello-world", 42, &comments);

        assert!(prompt.contains("octocat/hello-world"));
        assert!(prompt.contains("#42"));
    }
}
