//! Tests for the GitHub client, publication, and the end-to-end flow,
//! driven against a mocked API.

use super::*;
use crate::config::Config;
use crate::delta::CarriedFinding;
use crate::pipeline::RecordedProvider;
use crate::provider::ProviderRegistry;
use serde_json::json;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TOKEN: &str = "ghs-test-token";

fn client(server: &MockServer) -> GitHubClient {
    GitHubClient::new(&server.uri(), TOKEN.to_string(), "owner", "repo")
}

fn config() -> Config {
    Config::from_toml(&format!(
        r#"
[budget]
unlimited = true

[providers.openai]
family = "openai"
base_url = "https://provider.test/v1"
key_env = "TEST_KEY"

[models.triage]
provider = "openai"
name = "t"
input_price = 0.15
output_price = 0.60

[models.deep]
provider = "openai"
name = "d"
input_price = 3.00
output_price = 15.00

[models.verdict]
provider = "openai"
name = "t"
input_price = 0.15
output_price = 0.60
"#
    ))
    .unwrap()
}

fn prior_marker_body(head: &str, run_count: u32) -> String {
    let mut marker = Marker::new(head);
    marker.run_count = run_count;
    marker.spend.insert("triage".to_string(), 0.01);
    marker.findings.push(CarriedFinding {
        fingerprint: "fp-standing-blocker".to_string(),
        path: "src/old.rs".to_string(),
        start_line: 4,
        end_line: 4,
        severity: crate::config::Severity::Blocker,
        message: "unfixed sql injection".to_string(),
        harm: "Merging leaves the injection reachable by any user.".to_string(),
        state: crate::delta::CarriedState::Unresolved,
        first_seen: 1,
    });
    marker.encode()
}

const UNRELATED_DIFF: &str = "\
diff --git a/src/other.rs b/src/other.rs
--- a/src/other.rs
+++ b/src/other.rs
@@ -1,2 +1,3 @@
 fn other() {
+    let unrelated = true;
 }
";

/// Replies with each template in order, repeating the last, logging paths.
struct Seq {
    templates: Mutex<Vec<ResponseTemplate>>,
    log: Mutex<Vec<String>>,
    index: AtomicUsize,
}

impl Seq {
    fn new(templates: Vec<ResponseTemplate>, log: Mutex<Vec<String>>) -> Seq {
        Seq {
            templates: Mutex::new(templates),
            log,
            index: AtomicUsize::new(0),
        }
    }
}

impl Respond for Seq {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.log
            .lock()
            .unwrap()
            .push(request.url.path().to_string());
        let index = self.index.fetch_add(1, Ordering::SeqCst);
        let templates = self.templates.lock().unwrap();
        let last = templates.len().saturating_sub(1);
        templates[index.min(last)].clone()
    }
}

fn logged(responses: Vec<ResponseTemplate>, log: Mutex<Vec<String>>) -> Seq {
    Seq::new(responses, log)
}

#[tokio::test]
async fn client_fetches_pull_request_diff_and_reviews() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7"))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 7, "draft": false,
            "head": {"sha": "headsha1"}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7"))
        .and(header("accept", "application/vnd.github.v3.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string(UNRELATED_DIFF))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "user": {"login": "github-actions[bot]"}, "body": "older", "state": "COMMENTED"}
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let pr = client.pull_request(7).await.unwrap();
    assert_eq!(pr.head_sha(), "headsha1");
    assert!(!pr.draft);
    assert!(
        client
            .pull_request_diff(7)
            .await
            .unwrap()
            .contains("src/other.rs")
    );
    assert_eq!(client.reviews(7).await.unwrap().len(), 1);
    server.verify().await;
}

#[tokio::test]
async fn ancestry_check_distinguishes_fast_forward_from_force_push() {
    let server = MockServer::start().await;
    for (sha, status) in [("old1", "ahead"), ("old2", "diverged")] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/owner/repo/compare/{sha}...current")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": status})))
            .mount(&server)
            .await;
    }
    let client = client(&server);
    assert!(client.is_ancestor("old1", "current").await.unwrap());
    assert!(!client.is_ancestor("old2", "current").await.unwrap());
}

#[tokio::test]
async fn dismissed_fingerprints_read_from_resolved_threads() {
    let server = MockServer::start().await;
    let threads_json = r#"{
        "data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": [
            {"isResolved": true, "comments": {"nodes": [
                {"body": "looks fixed <!-- demur:fp fp-dismissed -->"}
            ]}},
            {"isResolved": false, "comments": {"nodes": [
                {"body": "still standing <!-- demur:fp fp-open -->"}
            ]}}
        ]}}}}}
    "#;
    let threads: serde_json::Value = serde_json::from_str(threads_json).unwrap();
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(threads))
        .mount(&server)
        .await;
    let client = client(&server);
    let dismissed = client.dismissed_fingerprints(7).await;
    assert!(dismissed.contains("fp-dismissed"));
    assert!(!dismissed.contains("fp-open"));
}

#[tokio::test]
async fn unreadable_resolution_state_treats_findings_as_unresolved() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(500).set_body_string("graphql down"))
        .mount(&server)
        .await;
    let client = client(&server);
    assert!(client.dismissed_fingerprints(7).await.is_empty());
}

#[tokio::test]
async fn missing_review_permission_fails_without_publishing_a_check_run() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/pulls/7/reviews"))
        .respond_with(
            ResponseTemplate::new(403).set_body_string("Resource not accessible by integration"),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/check-runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    let client = client(&server);
    let err = super::publish::publish_review(
        &client,
        7,
        "headsha",
        crate::pipeline::synthesis::Verdict::RequestChanges,
        "body",
        Some(&Marker::new("headsha")),
        &[],
    )
    .await
    .err()
    .expect("publication should fail");
    assert!(err.to_string().contains("permission"), "{err}");
    server.verify().await;
}

#[tokio::test]
async fn publication_posts_one_review_then_the_check_run() {
    let server = MockServer::start().await;
    let log = Mutex::new(Vec::new());
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 9})))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/check-runs"))
        .respond_with(logged(
            vec![ResponseTemplate::new(200).set_body_json(json!({}))],
            log,
        ))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    let publication = super::publish::publish_review(
        &client,
        7,
        "headsha",
        crate::pipeline::synthesis::Verdict::RequestChanges,
        "## review body",
        Some(&Marker::new("headsha")),
        &[],
    )
    .await
    .unwrap();
    assert!(!publication.fallback_used);
    assert_eq!(publication.event_submitted, ReviewEvent::RequestChanges);
    server.verify().await;
}

#[tokio::test]
async fn rejected_approval_falls_back_to_comment_review() {
    let server = MockServer::start().await;
    let bodies = std::sync::Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/pulls/7/reviews"))
        .respond_with(FallbackRecorder {
            calls: std::sync::Arc::clone(&bodies),
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/check-runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;
    let client = client(&server);
    let publication = super::publish::publish_review(
        &client,
        7,
        "headsha",
        crate::pipeline::synthesis::Verdict::Approve,
        "clean",
        Some(&Marker::new("headsha")),
        &[],
    )
    .await
    .unwrap();
    assert!(publication.fallback_used);
    assert_eq!(publication.event_submitted, ReviewEvent::Comment);
    let bodies = bodies.lock().unwrap();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0], "APPROVE");
    assert_eq!(bodies[1], "COMMENT");
    assert!(bodies[1].len() >= bodies[0].len());
}

struct FallbackRecorder {
    calls: std::sync::Arc<Mutex<Vec<String>>>,
}

impl Respond for FallbackRecorder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body = String::from_utf8(request.body.clone()).unwrap();
        let event = serde_json::from_str::<serde_json::Value>(&body).unwrap()["event"]
            .as_str()
            .unwrap()
            .to_string();
        let mut calls = self.calls.lock().unwrap();
        let first = calls.is_empty();
        calls.push(event);
        if first {
            ResponseTemplate::new(422)
                .set_body_string("Reviews may not be submitted with an approve state")
        } else {
            ResponseTemplate::new(200).set_body_json(json!({"id": 10}))
        }
    }
}

#[tokio::test]
async fn end_to_end_delta_review_carries_blocker_and_stays_red() {
    let server = MockServer::start().await;
    let prior_body = prior_marker_body("oldhead", 1);
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7"))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 7, "draft": false, "head": {"sha": "newhead"}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/compare/oldhead...newhead"))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "ahead"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/compare/oldhead...newhead"))
        .and(header("accept", "application/vnd.github.v3.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string(UNRELATED_DIFF))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "user": {"login": "github-actions[bot]"}, "body": prior_body, "state": "CHANGES_REQUESTED"}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": []}}}}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/pulls/7/reviews"))
        .respond_with(|request: &Request| {
            let body = String::from_utf8(request.body.clone()).unwrap();
            assert!(body.contains("unfixed sql injection"), "body: {body}");
            assert!(body.contains("demur:state"), "marker missing");
            ResponseTemplate::new(200).set_body_json(json!({"id": 11}))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/check-runs"))
        .respond_with(|request: &Request| {
            let body = String::from_utf8(request.body.clone()).unwrap();
            assert!(body.contains("\"failure\""), "check body: {body}");
            ResponseTemplate::new(200).set_body_json(json!({}))
        })
        .mount(&server)
        .await;

    let providers = ProviderRegistry::recorded(
        RecordedProvider::new(vec![Ok(json!({
            "findings": [],
            "cluster_lens": [{"path": "src/other.rs", "lenses": []}]
        }))]),
        RecordedProvider::new(vec![]),
        RecordedProvider::new(vec![Ok(
            json!({"summary": "A carried blocker still stands."}),
        )]),
    );
    let outcome = super::flow::review_pull_request(&client(&server), &providers, &config(), 7)
        .await
        .unwrap();
    assert!(outcome.published);
    assert_eq!(outcome.check_conclusion, "failure");
    assert!(outcome.summary.contains("unfixed sql injection"));
}

#[tokio::test]
async fn force_push_falls_back_to_a_full_review() {
    let server = MockServer::start().await;
    let prior_body = prior_marker_body("oldhead", 1);
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7"))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 7, "draft": false, "head": {"sha": "newhead"}
        })))
        .mount(&server)
        .await;
    // Force-push: prior head no longer an ancestor.
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/compare/oldhead...newhead"))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "diverged"})))
        .mount(&server)
        .await;
    // Full review therefore uses the pull request diff.
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7"))
        .and(header("accept", "application/vnd.github.v3.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string(UNRELATED_DIFF))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"id": 1, "user": {"login": "bot"}, "body": prior_body, "state": "CHANGES_REQUESTED"}
        ])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": []}}}}
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 12})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/check-runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(&server)
        .await;

    let providers = ProviderRegistry::recorded(
        RecordedProvider::new(vec![Ok(json!({
            "findings": [],
            "cluster_lens": [{"path": "src/other.rs", "lenses": []}]
        }))]),
        RecordedProvider::new(vec![]),
        RecordedProvider::new(vec![Ok(json!({"summary": "Full review complete."}))]),
    );
    let outcome = super::flow::review_pull_request(&client(&server), &providers, &config(), 7)
        .await
        .unwrap();
    assert!(outcome.published);
    // The carried blocker still stands even after history was rewritten.
    assert_eq!(outcome.check_conclusion, "failure");
}

fn too_large_body() -> String {
    r#"{"message":"Sorry, the diff exceeded the maximum number of lines (20000)","errors":[{"resource":"PullRequest","field":"diff","code":"too_large"}],"documentation_url":"https://docs.github.com/rest/pulls/pulls#get-a-pull-request","status":"406"}"#
    .to_string()
}

#[test]
fn synthesized_diff_covers_adds_removes_and_renames() {
    let files = vec![
        PrFile {
            path: "new.rs".to_string(),
            previous_path: None,
            status: "added".to_string(),
            patch: Some("@@ -0,0 +1,1 @@\n+fn fresh() {}".to_string()),
        },
        PrFile {
            path: "gone.rs".to_string(),
            previous_path: None,
            status: "removed".to_string(),
            patch: Some("@@ -1,1 +0,0 @@\n-fn stale() {}".to_string()),
        },
        PrFile {
            path: "moved.rs".to_string(),
            previous_path: Some("old.rs".to_string()),
            status: "renamed".to_string(),
            patch: Some("@@ -1,1 +1,1 @@\n fn a() {}".to_string()),
        },
        PrFile {
            path: "huge.bin".to_string(),
            previous_path: None,
            status: "modified".to_string(),
            patch: None,
        },
    ];
    let diff = synthesize_diff(&files);
    let parsed = crate::diff::parse_unified_diff(&diff);
    assert_eq!(parsed.len(), 3);
    assert_eq!(parsed[0].path, "new.rs");
    assert!(parsed[0].is_new);
    assert_eq!(parsed[1].path, "gone.rs");
    assert!(parsed[1].is_deleted);
    assert_eq!(parsed[2].path, "moved.rs");
    assert_eq!(parsed[2].old_path.as_deref(), Some("old.rs"));
}

#[tokio::test]
async fn oversized_pull_request_diff_falls_back_to_the_files_api() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7"))
        .and(header("accept", "application/vnd.github.v3.diff"))
        .respond_with(ResponseTemplate::new(406).set_body_string(too_large_body()))
        .expect(1)
        .mount(&server)
        .await;
    let page_one = json!([
        {"filename": "src/big.rs", "status": "modified",
         "patch": "@@ -1,2 +1,3 @@\n context\n+added line"},
        {"filename": "media/logo.png", "status": "modified", "patch": null}
    ]);
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7/files"))
        .and(wiremock::matchers::query_param("page", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page_one))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7/files"))
        .and(wiremock::matchers::query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let client = client(&server);
    let diff = client.pull_request_diff(7).await.unwrap();
    let parsed = crate::diff::parse_unified_diff(&diff);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].path, "src/big.rs");
    assert_eq!(parsed[0].hunks[0].added(), 1);
    server.verify().await;
}

#[tokio::test]
async fn oversized_delta_diff_falls_back_to_compare_files() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/compare/old...new"))
        .and(header("accept", "application/vnd.github.v3.diff"))
        .respond_with(ResponseTemplate::new(406).set_body_string(too_large_body()))
        .mount(&server)
        .await;
    let compare_files = json!({
        "files": [
            {"filename": "src/delta.rs", "status": "modified",
             "patch": "@@ -1,1 +1,2 @@\n fn a() {}\n+let b = 1;"}
        ]
    });
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/compare/old...new"))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(compare_files))
        .mount(&server)
        .await;

    let client = client(&server);
    let diff = client.compare_diff(7, "old", "new").await.unwrap();
    let parsed = crate::diff::parse_unified_diff(&diff);
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].path, "src/delta.rs");
    assert_eq!(parsed[0].hunks[0].added(), 1);
}
