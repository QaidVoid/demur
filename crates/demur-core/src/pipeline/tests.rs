//! Fixture harness: recorded provider responses drive end-to-end pipeline
//! tests for all three profiles, the budget gate, and the degradation
//! ladder.

use super::RunOutcome;
use crate::config::{Config, Severity};
use crate::diff::parse_unified_diff;
use crate::ingest::ingest;
use crate::pipeline::findings::Finding;
use crate::pipeline::prompt::PullRequestMeta;
use crate::pipeline::{PipelineError, PipelineInput, RecordedProvider};
use crate::provider::{ProviderError, ProviderRegistry};
use serde_json::json;

const DIFF: &str = "\
diff --git a/src/auth/token.rs b/src/auth/token.rs
--- a/src/auth/token.rs
+++ b/src/auth/token.rs
@@ -1,3 +1,5 @@
 fn issue() {
+    let hardcoded = \"sk-live-123\";
+    println!(\"{hardcoded}\");
 }
diff --git a/src/util.rs b/src/util.rs
--- a/src/util.rs
+++ b/src/util.rs
@@ -1,3 +1,4 @@
 fn util() {
+    let unused = 1;
 }
diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1,2 +1,3 @@
+[[package]]
 name = \"demur\"
";

fn config_with(profile: &str, overrides: &str) -> Config {
    let text = format!(
        r#"
profile = "{profile}"
{overrides}

[providers.openai]
family = "openai"
base_url = "https://api.openai.test/v1"
key_env = "TEST_KEY"

[models.triage]
provider = "openai"
name = "triage-model"
input_price = 0.15
output_price = 0.60

[models.deep]
provider = "openai"
name = "deep-model"
input_price = 3.00
output_price = 15.00

[models.verdict]
provider = "openai"
name = "verdict-model"
input_price = 0.15
output_price = 0.60
"#
    );
    Config::from_toml(&text).unwrap()
}

fn input() -> PipelineInput {
    let files = parse_unified_diff(DIFF);
    PipelineInput {
        meta: PullRequestMeta {
            title: "Add token".to_string(),
            description: "describe".to_string(),
            head_sha: "abc123".to_string(),
        },
        ingestion: ingest(&files, &config_with("standard", "")),
        diff_text: DIFF.to_string(),
        prior_spend: 0.0,
        carried_findings: Vec::new(),
        suppress_fingerprints: std::collections::HashSet::new(),
    }
}

fn triage_response() -> serde_json::Value {
    json!({
        "findings": [],
        "cluster_lens": [
            {"path": "src/auth/token.rs", "lenses": ["security"]},
            {"path": "src/util.rs", "lenses": ["correctness"]}
        ]
    })
}

fn dive_response(message: &str) -> serde_json::Value {
    json!({
        "findings": [{
            "file": "src/auth/token.rs",
            "start_line": 2,
            "end_line": 3,
            "severity": "blocker",
            "message": message,
            "harm": "Merging publishes a live credential reachable by attackers."
        }]
    })
}

fn summary_response() -> serde_json::Value {
    json!({"summary": "The credential exposure alone defeats the merge."})
}

fn registry(steps: Vec<Vec<serde_json::Value>>) -> ProviderRegistry {
    ProviderRegistry::recorded(
        RecordedProvider::new(steps[0].iter().map(|v| Ok(v.clone())).collect()),
        RecordedProvider::new(steps[1].iter().map(|v| Ok(v.clone())).collect()),
        RecordedProvider::new(steps[2].iter().map(|v| Ok(v.clone())).collect()),
    )
}

#[tokio::test]
async fn quick_profile_runs_triage_and_synthesis_only() {
    // One triage call, no deep calls, one verdict summary call.
    let config = config_with("quick", "");
    let providers = registry(vec![
        vec![triage_response()],
        vec![],
        vec![summary_response()],
    ]);
    let outcome = crate::pipeline::run(&providers, &config, &input())
        .await
        .unwrap();
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    assert_eq!(review.verdict, crate::pipeline::synthesis::Verdict::Approve);
    assert_eq!(review.spend.passes.len(), 2);
    assert_eq!(review.spend.passes[0].pass, "triage");
    assert_eq!(review.spend.passes[1].pass, "verdict summary");
}

#[tokio::test]
async fn standard_profile_adds_deep_dives_and_no_cross_examination() {
    let config = config_with("standard", "");
    let providers = registry(vec![
        vec![triage_response()],
        vec![
            dive_response("hardcoded credential"),
            dive_response("second"),
        ],
        vec![summary_response()],
    ]);
    let outcome = crate::pipeline::run(&providers, &config, &input())
        .await
        .unwrap();
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    assert_eq!(
        review.verdict,
        crate::pipeline::synthesis::Verdict::RequestChanges
    );
    let pass_names: Vec<&str> = review
        .spend
        .passes
        .iter()
        .map(|pass| pass.pass.as_str())
        .collect();
    assert_eq!(pass_names.len(), 4);
    assert_eq!(pass_names[1], "deep dive security");
    assert_eq!(pass_names[2], "deep dive correctness");
    assert!(!pass_names.contains(&"cross-examination"));
    assert!(review.body.contains("hardcoded credential"));
}

#[tokio::test]
async fn deep_profile_includes_cross_examination() {
    let config = config_with("deep", "");
    let cross = json!({
        "findings": [{
            "file": "src/util.rs",
            "start_line": 2,
            "end_line": 2,
            "severity": "warning",
            "message": "rollback breaks persistence",
            "harm": "Rolling back the deploy corrupts stored sessions."
        }]
    });
    let providers = registry(vec![
        vec![triage_response()],
        vec![
            dive_response("hardcoded credential"),
            dive_response("second"),
            cross,
        ],
        vec![summary_response()],
    ]);
    let outcome = crate::pipeline::run(&providers, &config, &input())
        .await
        .unwrap();
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    let pass_names: Vec<&str> = review
        .spend
        .passes
        .iter()
        .map(|pass| pass.pass.as_str())
        .collect();
    assert!(pass_names.contains(&"cross-examination"));
}

#[tokio::test]
async fn ceiling_cuts_deep_dives_in_risk_order_with_disclosure() {
    let config = config_with("standard", "[limits]\ndeep_calls = 1\ncomments = 10");
    let providers = registry(vec![
        vec![triage_response()],
        vec![dive_response("hardcoded credential")],
        vec![summary_response()],
    ]);
    let outcome = crate::pipeline::run(&providers, &config, &input())
        .await
        .unwrap();
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    let deep_calls = review
        .spend
        .passes
        .iter()
        .filter(|pass| pass.pass.starts_with("deep dive"))
        .count();
    assert_eq!(deep_calls, 1);
    assert!(review.body.contains("without a deep dive"));
    assert!(review.body.contains("src/util.rs"));
}

#[tokio::test]
async fn unanchored_and_praise_findings_never_publish() {
    let config = config_with("quick", "");
    let triage = json!({
        "findings": [
            {
                "file": "src/ghost.rs",
                "start_line": 1,
                "end_line": 2,
                "severity": "blocker",
                "message": "not in the diff",
                "harm": "Merging harms users somehow, definitely."
            },
            {
                "file": "src/util.rs",
                "start_line": 2,
                "end_line": 2,
                "severity": "note",
                "message": "great design",
                "harm": "This is an excellent elegant approach overall."
            }
        ],
        "cluster_lens": []
    });
    let providers = registry(vec![vec![triage], vec![], vec![summary_response()]]);
    let outcome = crate::pipeline::run(&providers, &config, &input())
        .await
        .unwrap();
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    assert!(review.published.is_empty());
    assert_eq!(review.verdict, crate::pipeline::synthesis::Verdict::Approve);
}

#[tokio::test]
async fn malformed_output_fails_the_run_without_a_review() {
    let config = config_with("quick", "");
    let providers = registry(vec![
        vec![json!({"findings": "not-an-array"})],
        vec![],
        vec![],
    ]);
    let err = crate::pipeline::run(&providers, &config, &input())
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        PipelineError::Provider(ProviderError::Malformed { .. })
    ));
}

#[tokio::test]
async fn exhausted_budget_degrades_to_summary_only_with_disclosure() {
    // A cap that funds triage and the summary but no deep dive at deep
    // prices; the downgrade rung is cheaper than the deep price and fits,
    // so the dives run degraded instead of skipping.
    let config = config_with("standard", "[budget]\nper_pr_usd = 0.004\n");
    let providers = registry(vec![
        vec![triage_response()],
        vec![dive_response("hardcoded credential")],
        vec![summary_response()],
    ]);
    let outcome = crate::pipeline::run(&providers, &config, &input())
        .await
        .unwrap();
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    let described: Vec<String> = review
        .degradations
        .iter()
        .map(|degradation| degradation.describe())
        .collect();
    assert!(
        described
            .iter()
            .any(|text| text.contains("triage model instead of the deep model")),
        "degradations were: {described:?}"
    );
    assert!(review.body.contains("Coverage and spend"));
}

#[tokio::test]
async fn fully_consumed_cap_skips_with_notice_and_no_review() {
    let config = config_with("standard", "[budget]\nper_pr_usd = 1.0\n");
    let mut consumed = input();
    consumed.prior_spend = 1.0;
    let providers = registry(vec![vec![], vec![], vec![]]);
    let outcome = crate::pipeline::run(&providers, &config, &consumed)
        .await
        .unwrap();
    let RunOutcome::Skipped(notice) = outcome else {
        panic!("expected a skipped run");
    };
    assert!(notice.contains("Review skipped"));
    assert!(notice.contains("1.0000"));
}

#[tokio::test]
async fn truncated_output_escalates_the_ceiling_and_succeeds() {
    use crate::config::{Family, ModelDef, ProviderDef};
    use crate::provider::{AnyProvider, OpenAiClient};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    // First attempts run at the configured 2000-token ceiling and come
    // back truncated; the 4x retries succeed.
    struct Escalation(std::sync::Mutex<Vec<u64>>);

    impl Respond for Escalation {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let ceiling = body["max_tokens"].as_u64().unwrap();
            self.0.lock().unwrap().push(ceiling);
            if ceiling <= 2000 {
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"content": ""}, "finish_reason": "length"}],
                    "usage": {"prompt_tokens": 50, "completion_tokens": 2000}
                }));
            }
            let valid = if self.0.lock().unwrap().iter().filter(|c| **c > 2000).count() == 1 {
                serde_json::json!({
                    "findings": [],
                    "cluster_lens": [{"path": "src/auth/token.rs", "lenses": []},
                                     {"path": "src/util.rs", "lenses": []}]
                })
            } else {
                serde_json::json!({"summary": "Nothing to argue against."})
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": valid.to_string()}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 50, "completion_tokens": 10}
            }))
        }
    }

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(Escalation(std::sync::Mutex::new(Vec::new())))
        .mount(&server)
        .await;

    let provider = ProviderDef {
        family: Family::OpenAi,
        base_url: server.uri(),
        key_env: "TEST".to_string(),
        key_file: None,
        extra_body: None,
        extra_headers: None,
    };
    let model = ModelDef {
        provider: "test".to_string(),
        name: "m".to_string(),
        input_price: 1.0,
        output_price: 2.0,
        reasoning_effort: None,
        thinking_budget: None,
        extra_body: None,
        extra_headers: None,
        cached_input_price: None,
    };
    let make =
        || AnyProvider::OpenAi(OpenAiClient::new(&provider, &model, "k".to_string()).unwrap());
    let registry = ProviderRegistry {
        triage: make(),
        deep: make(),
        verdict: make(),
    };
    let config = config_with("quick", "");
    let outcome = crate::pipeline::run(&registry, &config, &input())
        .await
        .unwrap();
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    assert_eq!(review.verdict, crate::pipeline::synthesis::Verdict::Approve);
}

#[tokio::test]
async fn carried_blocker_sets_the_verdict_on_a_clean_delta() {
    let config = config_with("standard", "");
    let triage = json!({
        "findings": [],
        "cluster_lens": [
            {"path": "src/auth/token.rs", "lenses": []},
            {"path": "src/util.rs", "lenses": []}
        ]
    });
    let carried = Finding {
        file: "src/old.rs".to_string(),
        start_line: 4,
        end_line: 4,
        severity: Severity::Blocker,
        message: "unfixed sql injection".to_string(),
        harm: "Merging leaves the injection reachable by any user.".to_string(),
        suggestion: None,
    };
    let mut run_input = input();
    run_input.carried_findings = vec![carried];
    let _ = &run_input;
    // Empty lens lists mean no deep dives: a clean delta.
    let providers = registry(vec![vec![triage], vec![], vec![summary_response()]]);
    let outcome = crate::pipeline::run(&providers, &config, &run_input)
        .await
        .unwrap();
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    assert_eq!(
        review.verdict,
        crate::pipeline::synthesis::Verdict::RequestChanges
    );
    assert!(
        review
            .published
            .iter()
            .any(|finding| finding.message == "unfixed sql injection")
    );
}

fn registry_results(
    triage: Vec<Result<serde_json::Value, ProviderError>>,
    deep: Vec<Result<serde_json::Value, ProviderError>>,
    verdict: Vec<Result<serde_json::Value, ProviderError>>,
) -> ProviderRegistry {
    ProviderRegistry::recorded(
        RecordedProvider::new(triage),
        RecordedProvider::new(deep),
        RecordedProvider::new(verdict),
    )
}

fn recorded(provider: &crate::provider::AnyProvider) -> &RecordedProvider {
    match provider {
        crate::provider::AnyProvider::Recorded(inner) => inner,
        _ => panic!("expected a recorded provider"),
    }
}

#[tokio::test]
async fn verdict_summary_failure_still_publishes_the_completed_review() {
    // The summary is prose. Losing it must not discard the deep dives that
    // were already paid for, which is what a rejected request used to do.
    let config = config_with("standard", "");
    let providers = registry_results(
        vec![Ok(triage_response())],
        vec![
            Ok(dive_response("hardcoded credential")),
            Ok(dive_response("second")),
        ],
        vec![Err(ProviderError::Rejected {
            message: "failed to download media at input[0].content[0]".to_string(),
        })],
    );
    let outcome = crate::pipeline::run(&providers, &config, &input())
        .await
        .expect("a rejected summary must not fail the run");
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    assert_eq!(
        review.verdict,
        crate::pipeline::synthesis::Verdict::RequestChanges
    );
    assert!(review.body.contains("hardcoded credential"));
    assert!(
        review.body.contains("verdict summary could not be drafted"),
        "the failure must be disclosed: {}",
        review.body
    );
    // The deep dive spend survives so the review can still report it.
    assert!(review.spend.total > 0.0);
}

#[tokio::test]
async fn deep_dive_failure_degrades_and_keeps_the_other_dives() {
    let config = config_with("standard", "");
    let providers = registry_results(
        vec![Ok(triage_response())],
        vec![
            Ok(dive_response("hardcoded credential")),
            Err(ProviderError::Rejected {
                message: "provider said no".to_string(),
            }),
        ],
        vec![Ok(summary_response())],
    );
    let outcome = crate::pipeline::run(&providers, &config, &input())
        .await
        .expect("one failed dive must not fail the run");
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    assert!(review.body.contains("hardcoded credential"));
    assert!(
        review.body.contains("failed and was skipped"),
        "the skipped dive must be disclosed: {}",
        review.body
    );
}

#[tokio::test]
async fn auth_failure_on_a_deep_dive_fails_the_run_immediately() {
    // A bad key repeats on every cluster, so degrading would burn the
    // whole ceiling to learn what the first call already proved.
    let config = config_with("standard", "");
    let providers = registry_results(
        vec![Ok(triage_response())],
        vec![Err(ProviderError::Auth {
            message: "invalid key".to_string(),
        })],
        vec![Ok(summary_response())],
    );
    let error = crate::pipeline::run(&providers, &config, &input())
        .await
        .expect_err("an auth failure must stop the run");
    let PipelineError::Provider(ProviderError::Auth { .. }) = error else {
        panic!("expected an auth failure, got {error}");
    };
}

#[tokio::test]
async fn budget_downgrade_sends_the_pass_to_the_triage_model() {
    // The deep provider is given no steps at all: if the downgrade rung is
    // disclosed but the call still goes to the deep model, the run fails.
    let config = config_with("standard", "[budget]\nper_pr_usd = 0.01");
    let providers = registry(vec![
        vec![
            triage_response(),
            dive_response("hardcoded credential"),
            dive_response("second"),
        ],
        vec![],
        vec![summary_response()],
    ]);
    let outcome = crate::pipeline::run(&providers, &config, &input())
        .await
        .expect("the downgraded dives run on the triage model");
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    assert!(
        review
            .body
            .contains("ran on the triage model instead of the deep model"),
        "{}",
        review.body
    );
    assert!(
        recorded(&providers.deep).requests().is_empty(),
        "a downgraded pass must not reach the deep model"
    );
}
