use git_automate::external_issues::github::client::GitHubClient;
use git_automate::external_issues::github::pr_comments::PRCommentService;
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn e2e_service_build_fix_prompt() {
    let mock = MockServer::start().await;
    let client = GitHubClient::new_with_base_url("test-token".to_string(), mock.uri()).unwrap();
    let service = PRCommentService::new(&client);

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("reviewThreads"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviewThreads": {
                            "nodes": [{
                                "id": "thread-1",
                                "path": "src/main.rs",
                                "line": 42,
                                "originalLine": 38,
                                "isResolved": false,
                                "diffSide": "RIGHT",
                                "comments": {
                                    "nodes": [{
                                        "id": "comment-1",
                                        "body": "Handle errors properly",
                                        "createdAt": "2024-01-01T00:00:00Z",
                                        "author": {"login": "reviewer"}
                                    }]
                                }
                            }]
                        }
                    }
                }
            }
        })))
        .mount(&mock)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("ListPrComments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "comments": {
                            "nodes": [{
                                "id": "comment-2",
                                "body": "Nice work overall",
                                "createdAt": "2024-01-02T00:00:00Z",
                                "author": {"login": "reviewer2"}
                            }]
                        }
                    }
                }
            }
        })))
        .mount(&mock)
        .await;

    let prompt = service
        .build_fix_prompt("octocat", "hello-world", 42)
        .await
        .expect("should succeed");
    println!("=== E2E PROMPT ===\n{}", prompt);

    assert!(prompt.contains("octocat/hello-world"));
    assert!(prompt.contains("#42"));
    assert!(prompt.contains("src/main.rs"));
    assert!(prompt.contains("Line: 42"));
    assert!(prompt.contains("Handle errors properly"));
    assert!(prompt.contains("Thread ID: thread-1"));
    assert!(prompt.contains("Nice work overall"));
    assert!(prompt.contains("reviewer2"));
    assert!(prompt.contains("Instructions"));
}
