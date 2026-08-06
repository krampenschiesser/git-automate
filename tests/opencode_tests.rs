//! OpenCode client integration tests.
//!
//! Test 3 from the original `integration.rs`: health check failover
//! across 500, `{"healthy": false}`, and `{"healthy": true}`.

mod common;

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::oc_client;

// ─── Test 3: OpenCode health check failover ───────────────────

#[tokio::test]
async fn test_opencode_health_check_failover() {
    // 500 → false
    {
        let server = MockServer::start().await;
        let client = oc_client(&server);

        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert!(!client.check_health().await, "500 should return false");
    }

    // {"healthy": false} → false
    {
        let server = MockServer::start().await;
        let client = oc_client(&server);

        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "healthy": false,
                "version": "1.0"
            })))
            .mount(&server)
            .await;

        assert!(
            !client.check_health().await,
            "healthy=false should return false"
        );
    }

    // {"healthy": true, "version": "1.0"} → true
    {
        let server = MockServer::start().await;
        let client = oc_client(&server);

        Mock::given(method("GET"))
            .and(path("/global/health"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "healthy": true,
                "version": "1.0"
            })))
            .mount(&server)
            .await;

        assert!(
            client.check_health().await,
            "healthy=true should return true"
        );
    }
}
