//! Fixture harness: recorded provider responses drive end-to-end pipeline
//! tests for all three profiles, the budget gate, and the degradation
//! ladder.

use super::RunOutcome;
use super::prompt::MetaOrigin;
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
            origin: MetaOrigin::PullRequest,
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
    let RunOutcome::Skipped { notice, .. } = outcome else {
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

fn config_with_rules(rules: &str) -> Config {
    config_with("standard", rules)
}

#[tokio::test]
async fn metadata_violations_reach_the_review_without_a_provider_call() {
    let config =
        config_with_rules("[review.title]\npattern = '^(feat|fix): .+'\nseverity = \"blocker\"");
    let providers = registry(vec![
        vec![triage_response()],
        vec![dive_response("a"), dive_response("b")],
        vec![summary_response()],
    ]);
    let mut input = input();
    input.meta.title = "added a token".to_string();
    let outcome = crate::pipeline::run(&providers, &config, &input)
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
        review.body.contains("pull request title"),
        "{}",
        review.body
    );
    assert!(
        review.body.contains("does not match the required format"),
        "{}",
        review.body
    );
    // The location names the field, never an invented line number.
    assert!(
        !review.body.contains("pull request title:0"),
        "{}",
        review.body
    );
}

#[tokio::test]
async fn a_satisfied_rule_raises_nothing() {
    let config = config_with_rules("[review.title]\npattern = '^(feat|fix): .+'");
    let providers = registry(vec![
        vec![triage_response()],
        vec![dive_response("a"), dive_response("b")],
        vec![summary_response()],
    ]);
    let mut input = input();
    input.meta.title = "feat: add a token".to_string();
    let outcome = crate::pipeline::run(&providers, &config, &input)
        .await
        .unwrap();
    let RunOutcome::Review(review) = outcome else {
        panic!("expected a review");
    };
    assert!(
        !review.body.contains("pull request title"),
        "{}",
        review.body
    );
}

#[tokio::test]
async fn violations_survive_a_run_that_can_fund_no_pass() {
    // The budget is consumed before the run starts. Rules cost nothing, so
    // the skipped run still reports what it established.
    let config = config_with_rules(
        "[budget]\nper_pr_usd = 0.000001\n\n[review.description]\nrequired = true\nseverity = \"blocker\"",
    );
    let providers = registry(vec![vec![], vec![], vec![]]);
    let mut input = input();
    input.meta.description = String::new();
    let outcome = crate::pipeline::run(&providers, &config, &input)
        .await
        .unwrap();
    let RunOutcome::Skipped { notice, violations } = outcome else {
        panic!("expected a skipped run");
    };
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].severity, Severity::Blocker);
    assert!(
        notice.contains("Rule violations found without a provider call"),
        "{notice}"
    );
    assert!(notice.contains("pull request description"), "{notice}");
}

#[tokio::test]
async fn an_unapplicable_rule_never_reaches_a_run() {
    // Configuration validation rejects it first, but the pipeline refuses
    // it too rather than silently reviewing without the rule.
    let mut config = config_with("standard", "");
    config.review.title.pattern = Some("(unclosed".to_string());
    let providers = registry(vec![vec![], vec![], vec![]]);
    let error = crate::pipeline::run(&providers, &config, &input())
        .await
        .expect_err("an uncompilable rule must stop the run");
    assert!(
        matches!(error, PipelineError::Rules(_)),
        "expected a rules failure, got {error}"
    );
}

fn cached_config(dir: &std::path::Path) -> Config {
    let mut config = config_with("standard", "");
    config.cache.enabled = true;
    config.cache.dir = Some(dir.to_path_buf());
    config
}

fn steps() -> Vec<Vec<serde_json::Value>> {
    vec![
        vec![triage_response()],
        vec![
            dive_response("hardcoded credential"),
            dive_response("second"),
        ],
        vec![summary_response()],
    ]
}

#[tokio::test]
async fn a_fully_cached_run_and_a_cold_run_agree_exactly() {
    // This is the guarantee the whole capability rests on: a cache may
    // change what a run costs and nothing else.
    let dir = tempfile::tempdir().unwrap();
    let config = cached_config(dir.path());

    let cold = crate::pipeline::run(&registry(steps()), &config, &input())
        .await
        .unwrap();
    let RunOutcome::Review(cold) = cold else {
        panic!("expected a review");
    };

    // Every pass is now cached. A provider with no steps left proves no
    // call is made: any miss would fail with "no more recorded steps".
    let empty = registry(vec![vec![], vec![], vec![]]);
    let warm = crate::pipeline::run(&empty, &config, &input())
        .await
        .expect("a fully cached run makes no provider call");
    let RunOutcome::Review(warm) = warm else {
        panic!("expected a review");
    };

    assert_eq!(cold.verdict, warm.verdict);
    assert_eq!(cold.published.len(), warm.published.len());
    for (a, b) in cold.published.iter().zip(warm.published.iter()) {
        assert_eq!(a.message, b.message);
        assert_eq!(a.severity, b.severity);
        assert_eq!(a.file, b.file);
        assert_eq!(a.start_line, b.start_line);
    }
    assert_eq!(cold.omitted, warm.omitted);
    assert_eq!(cold.degradations, warm.degradations);
}

#[tokio::test]
async fn a_resumed_run_pays_nothing_and_says_what_it_inherited() {
    let dir = tempfile::tempdir().unwrap();
    let config = cached_config(dir.path());
    crate::pipeline::run(&registry(steps()), &config, &input())
        .await
        .unwrap();

    let empty = registry(vec![vec![], vec![], vec![]]);
    let RunOutcome::Review(warm) = crate::pipeline::run(&empty, &config, &input())
        .await
        .unwrap()
    else {
        panic!("expected a review");
    };
    assert_eq!(warm.spend.paid, 0.0, "a fully resumed run pays nothing");
    assert!(
        warm.spend.inherited > 0.0,
        "the earlier attempt's cost still counts"
    );
    assert_eq!(warm.spend.total, warm.spend.inherited);
    assert!(
        warm.body.contains("Inherited from an earlier attempt"),
        "{}",
        warm.body
    );
    assert!(warm.body.contains("resumed from cache"), "{}", warm.body);
    assert!(
        warm.spend.passes.iter().all(|pass| pass.resumed),
        "every pass was resumed"
    );
}

#[tokio::test]
async fn nothing_is_cached_unless_it_is_asked_for() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config_with("standard", "");
    config.cache.dir = Some(dir.path().join("unused"));
    // enabled stays false.
    crate::pipeline::run(&registry(steps()), &config, &input())
        .await
        .unwrap();
    assert!(
        !dir.path().join("unused").exists(),
        "a disabled cache creates no location"
    );
}

#[tokio::test]
async fn a_failed_pass_leaves_nothing_to_resume() {
    let dir = tempfile::tempdir().unwrap();
    let config = cached_config(dir.path());
    let providers = registry_results(
        vec![Ok(triage_response())],
        vec![
            Ok(dive_response("hardcoded credential")),
            Err(ProviderError::Rejected {
                message: "no".to_string(),
            }),
        ],
        vec![Ok(summary_response())],
    );
    crate::pipeline::run(&providers, &config, &input())
        .await
        .unwrap();

    // The second dive failed, so a retry must call the provider for it
    // again. Only the successful passes are served from cache.
    let retry = registry_results(vec![], vec![Ok(dive_response("second"))], vec![]);
    let RunOutcome::Review(review) = crate::pipeline::run(&retry, &config, &input())
        .await
        .expect("the retry resumes what completed and re-runs what failed")
    else {
        panic!("expected a review");
    };
    assert!(review.body.contains("second"), "{}", review.body);
    assert!(
        review.spend.paid > 0.0,
        "the failed pass had to be paid for on the retry"
    );
}

#[tokio::test]
async fn a_changed_model_is_not_served_from_cache() {
    let dir = tempfile::tempdir().unwrap();
    let config = cached_config(dir.path());
    crate::pipeline::run(&registry(steps()), &config, &input())
        .await
        .unwrap();

    let mut changed = cached_config(dir.path());
    changed.models.deep.name = "a-different-deep-model".to_string();
    // The deep provider must be called again; entries from the old model
    // answer a question this run is not asking.
    let providers = registry(vec![
        vec![],
        vec![
            dive_response("hardcoded credential"),
            dive_response("second"),
        ],
        vec![],
    ]);
    crate::pipeline::run(&providers, &changed, &input())
        .await
        .expect("a changed model re-runs the deep dives");
}

#[tokio::test]
async fn an_unusable_cache_location_runs_cold_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a-file");
    std::fs::write(&file, "not a directory").unwrap();
    let config = cached_config(&file.join("under"));
    crate::pipeline::run(&registry(steps()), &config, &input())
        .await
        .expect("an unusable cache location must not fail the run");
}

fn local_input() -> PipelineInput {
    let mut input = input();
    // What the local CLI supplies: a label demur wrote, and the range's
    // commit messages.
    input.meta.title = "local review: HEAD~1..HEAD".to_string();
    input.meta.description = "feat: add b".to_string();
    input.meta.origin = super::prompt::MetaOrigin::Composed;
    input
}

const STRICT_RULES: &str = "[review.title]\npattern = '^(feat|fix): .+'\nseverity = \"blocker\"\n\n[review.description]\nrequired = true\nmin_length = 40\nseverity = \"blocker\"";

#[tokio::test]
async fn a_local_range_review_raises_no_metadata_violation() {
    // Both the composed title and the short description would violate
    // these rules. Neither is a claim the author made.
    let config = config_with("standard", STRICT_RULES);
    let providers = registry(vec![
        vec![triage_response()],
        vec![dive_response("a"), dive_response("b")],
        vec![summary_response()],
    ]);
    let RunOutcome::Review(review) = crate::pipeline::run(&providers, &config, &local_input())
        .await
        .unwrap()
    else {
        panic!("expected a review");
    };
    // Assert on violations, not on the words: the line explaining that
    // rules did not run mentions the title too.
    assert!(
        review
            .published
            .iter()
            .all(|finding| !finding.file.starts_with("pull request")),
        "no metadata violation may be published: {:?}",
        review.published
    );
    assert!(
        !review.body.contains("does not match the required format"),
        "{}",
        review.body
    );
    assert!(!review.body.contains("is empty"), "{}", review.body);
    // The verdict comes from the code findings alone, exactly as it would
    // with no rules configured.
    let without_rules = registry(vec![
        vec![triage_response()],
        vec![dive_response("a"), dive_response("b")],
        vec![summary_response()],
    ]);
    let RunOutcome::Review(bare) =
        crate::pipeline::run(&without_rules, &config_with("standard", ""), &local_input())
            .await
            .unwrap()
    else {
        panic!("expected a review");
    };
    assert_eq!(review.verdict, bare.verdict);
    assert_eq!(review.published.len(), bare.published.len());
}

#[tokio::test]
async fn the_same_rules_still_bite_on_a_pull_request() {
    let config = config_with("standard", STRICT_RULES);
    let providers = registry(vec![
        vec![triage_response()],
        vec![dive_response("a"), dive_response("b")],
        vec![summary_response()],
    ]);
    let mut input = input();
    input.meta.title = "added a token".to_string();
    input.meta.description = String::new();
    let RunOutcome::Review(review) = crate::pipeline::run(&providers, &config, &input)
        .await
        .unwrap()
    else {
        panic!("expected a review");
    };
    assert_eq!(
        review.verdict,
        crate::pipeline::synthesis::Verdict::RequestChanges
    );
    assert!(
        review.body.contains("pull request title"),
        "{}",
        review.body
    );
    assert!(
        review.body.contains("pull request description"),
        "{}",
        review.body
    );
}

#[tokio::test]
async fn a_local_run_says_rules_were_not_evaluated() {
    let config = config_with("standard", STRICT_RULES);
    let providers = registry(vec![
        vec![triage_response()],
        vec![dive_response("a"), dive_response("b")],
        vec![summary_response()],
    ]);
    let RunOutcome::Review(review) = crate::pipeline::run(&providers, &config, &local_input())
        .await
        .unwrap()
    else {
        panic!("expected a review");
    };
    assert!(
        review.body.contains("Metadata rules were not evaluated"),
        "a configured rule that did not run must say so: {}",
        review.body
    );
    // Stated once, and it is not a finding.
    assert_eq!(
        review
            .body
            .matches("Metadata rules were not evaluated")
            .count(),
        1
    );
    assert!(
        review
            .published
            .iter()
            .all(|f| f.file != "pull request title")
    );
}

#[tokio::test]
async fn a_local_run_without_rules_says_nothing_about_them() {
    let config = config_with("standard", "");
    let providers = registry(vec![
        vec![triage_response()],
        vec![dive_response("a"), dive_response("b")],
        vec![summary_response()],
    ]);
    let RunOutcome::Review(review) = crate::pipeline::run(&providers, &config, &local_input())
        .await
        .unwrap()
    else {
        panic!("expected a review");
    };
    assert!(!review.body.contains("Metadata rules"), "{}", review.body);
}

#[tokio::test]
async fn a_skipped_local_run_reports_no_violation_but_explains_itself() {
    let config = config_with(
        "standard",
        "[budget]\nper_pr_usd = 0.000001\n\n[review.description]\nrequired = true\nseverity = \"blocker\"",
    );
    let providers = registry(vec![vec![], vec![], vec![]]);
    let RunOutcome::Skipped { notice, violations } =
        crate::pipeline::run(&providers, &config, &local_input())
            .await
            .unwrap()
    else {
        panic!("expected a skipped run");
    };
    assert!(violations.is_empty(), "no rule applied to a local range");
    assert!(!notice.contains("Rule violations found"), "{notice}");
    assert!(
        notice.contains("Metadata rules were not evaluated"),
        "{notice}"
    );
}

/// A deep dive answer naming the cluster it came from, so a test can tell
/// which dive produced which finding.
fn keyed_dive(file: &str, message: &str) -> serde_json::Value {
    json!({
        "findings": [{
            "file": file,
            "start_line": 2,
            "end_line": 2,
            "severity": "warning",
            "message": message,
            "harm": "Merging this leaves the defect reachable in production."
        }]
    })
}

/// Deep dives that deliberately finish in the opposite order to the one
/// they were started in.
fn out_of_order_registry() -> ProviderRegistry {
    use std::time::Duration;
    ProviderRegistry::recorded(
        RecordedProvider::new(vec![Ok(triage_response())]),
        RecordedProvider::keyed(vec![
            // The first cluster planned answers last.
            (
                "src/auth/token.rs".to_string(),
                Ok(keyed_dive("src/auth/token.rs", "token defect")),
                Duration::from_millis(120),
            ),
            (
                "src/util.rs".to_string(),
                Ok(keyed_dive("src/util.rs", "util defect")),
                Duration::from_millis(10),
            ),
        ]),
        RecordedProvider::new(vec![Ok(summary_response())]),
    )
}

fn concurrent_config(limit: u32) -> Config {
    let mut config = config_with("standard", "");
    config.limits.concurrency = limit;
    config
}

#[tokio::test]
async fn the_dive_planned_first_wins_deduplication() {
    // Two lenses on one file report the same defect at different
    // severities. Deduplication keeps the first occurrence, so if results
    // were collected as they arrived, the slower lens would lose its
    // severity and the verdict would follow whichever call happened to
    // return first.
    use std::time::Duration;
    let triage = json!({
        "findings": [],
        "cluster_lens": [{"path": "src/auth/token.rs", "lenses": ["security", "correctness"]}]
    });
    let same_defect = |severity: &str| {
        json!({
            "findings": [{
                "file": "src/auth/token.rs",
                "start_line": 2,
                "end_line": 3,
                "severity": severity,
                "message": "hardcoded credential",
                "harm": "Merging publishes a live credential reachable by attackers."
            }]
        })
    };
    let providers = ProviderRegistry::recorded(
        RecordedProvider::new(vec![Ok(triage)]),
        RecordedProvider::keyed(vec![
            // Planned first, answers last.
            (
                "through the security lens".to_string(),
                Ok(same_defect("blocker")),
                Duration::from_millis(120),
            ),
            (
                "through the correctness lens".to_string(),
                Ok(same_defect("note")),
                Duration::from_millis(5),
            ),
        ]),
        RecordedProvider::new(vec![Ok(summary_response())]),
    );
    let RunOutcome::Review(review) =
        crate::pipeline::run(&providers, &concurrent_config(4), &input())
            .await
            .unwrap()
    else {
        panic!("expected a review");
    };
    assert_eq!(review.published.len(), 1, "the two are one finding");
    assert_eq!(
        review.published[0].severity,
        Severity::Blocker,
        "the dive planned first must win, whatever order they returned in"
    );
    assert_eq!(
        review.verdict,
        crate::pipeline::synthesis::Verdict::RequestChanges
    );
}

#[tokio::test]
async fn disclosed_spend_follows_the_planned_order() {
    // The spend lines are rendered in the order passes were planned, so a
    // review of the same pull request always reads the same way.
    let RunOutcome::Review(review) =
        crate::pipeline::run(&out_of_order_registry(), &concurrent_config(4), &input())
            .await
            .unwrap()
    else {
        panic!("expected a review");
    };
    let dives: Vec<&str> = review
        .spend
        .passes
        .iter()
        .map(|pass| pass.pass.as_str())
        .filter(|name| name.starts_with("deep dive"))
        .collect();
    assert_eq!(
        dives,
        vec!["deep dive security", "deep dive correctness"],
        "spend must follow the planned order, not the completion order"
    );
}

#[tokio::test]
async fn a_concurrent_run_and_a_serial_run_agree_exactly() {
    let serial = crate::pipeline::run(&out_of_order_registry(), &concurrent_config(1), &input())
        .await
        .unwrap();
    let concurrent =
        crate::pipeline::run(&out_of_order_registry(), &concurrent_config(4), &input())
            .await
            .unwrap();
    let (RunOutcome::Review(serial), RunOutcome::Review(concurrent)) = (serial, concurrent) else {
        panic!("expected reviews");
    };
    assert_eq!(serial.verdict, concurrent.verdict);
    assert_eq!(serial.omitted, concurrent.omitted);
    assert_eq!(serial.degradations, concurrent.degradations);
    assert_eq!(serial.published.len(), concurrent.published.len());
    for (a, b) in serial.published.iter().zip(concurrent.published.iter()) {
        assert_eq!(a.file, b.file);
        assert_eq!(a.message, b.message);
        assert_eq!(a.severity, b.severity);
    }
    assert!(
        (serial.spend.total - concurrent.spend.total).abs() < 1e-12,
        "concurrency must not change spend: {} vs {}",
        serial.spend.total,
        concurrent.spend.total
    );
    assert_eq!(serial.body, concurrent.body);
}

#[tokio::test]
async fn concurrency_of_one_is_the_serial_path() {
    let RunOutcome::Review(review) =
        crate::pipeline::run(&out_of_order_registry(), &concurrent_config(1), &input())
            .await
            .unwrap()
    else {
        panic!("expected a review");
    };
    let order: Vec<&str> = review
        .published
        .iter()
        .map(|finding| finding.file.as_str())
        .collect();
    assert_eq!(order, vec!["src/auth/token.rs", "src/util.rs"]);
}

#[tokio::test]
async fn the_deep_call_ceiling_bounds_what_is_launched() {
    let mut config = concurrent_config(4);
    config.limits.deep_calls = 1;
    let providers = out_of_order_registry();
    let RunOutcome::Review(review) = crate::pipeline::run(&providers, &config, &input())
        .await
        .unwrap()
    else {
        panic!("expected a review");
    };
    let dives = review
        .spend
        .passes
        .iter()
        .filter(|pass| pass.pass.starts_with("deep dive"))
        .count();
    assert_eq!(dives, 1, "the ceiling bounds launches, not completions");
    assert!(
        review.body.contains("deep call ceiling left"),
        "{}",
        review.body
    );
}

#[tokio::test]
async fn one_concurrent_failure_keeps_the_other_dives() {
    use std::time::Duration;
    let providers = ProviderRegistry::recorded(
        RecordedProvider::new(vec![Ok(triage_response())]),
        RecordedProvider::keyed(vec![
            (
                "src/auth/token.rs".to_string(),
                Err(ProviderError::Rejected {
                    message: "provider said no".to_string(),
                }),
                Duration::from_millis(5),
            ),
            (
                "src/util.rs".to_string(),
                Ok(keyed_dive("src/util.rs", "util defect")),
                Duration::from_millis(40),
            ),
        ]),
        RecordedProvider::new(vec![Ok(summary_response())]),
    );
    let RunOutcome::Review(review) =
        crate::pipeline::run(&providers, &concurrent_config(4), &input())
            .await
            .unwrap()
    else {
        panic!("expected a review");
    };
    assert!(review.body.contains("util defect"), "{}", review.body);
    assert!(
        review.body.contains("failed and was skipped"),
        "{}",
        review.body
    );
}

/// Six dives over three clusters, each slow enough to overlap.
fn many_dives_registry() -> ProviderRegistry {
    use std::time::Duration;
    let triage = json!({
        "findings": [],
        "cluster_lens": [
            {"path": "src/auth/token.rs", "lenses": ["security", "correctness"]},
            {"path": "src/util.rs", "lenses": ["security", "correctness"]}
        ]
    });
    ProviderRegistry::recorded(
        RecordedProvider::new(vec![Ok(triage)]),
        RecordedProvider::keyed(vec![
            (
                "through the security lens".to_string(),
                Ok(keyed_dive("src/auth/token.rs", "security defect")),
                Duration::from_millis(60),
            ),
            (
                "through the correctness lens".to_string(),
                Ok(keyed_dive("src/util.rs", "correctness defect")),
                Duration::from_millis(60),
            ),
            (
                "Cross-examine".to_string(),
                Ok(json!({"findings": []})),
                Duration::from_millis(5),
            ),
        ]),
        RecordedProvider::new(vec![Ok(summary_response())]),
    )
}

#[tokio::test]
async fn in_flight_calls_never_exceed_the_limit() {
    for limit in [1u32, 2, 3] {
        let providers = many_dives_registry();
        let mut config = concurrent_config(limit);
        config.limits.deep_calls = 4;
        crate::pipeline::run(&providers, &config, &input())
            .await
            .unwrap();
        let deep = recorded(&providers.deep);
        assert_eq!(
            deep.requests().len(),
            4,
            "every planned dive must run at limit {limit}"
        );
        assert!(
            deep.peak_in_flight() <= limit as usize,
            "limit {limit} exceeded: peak was {}",
            deep.peak_in_flight()
        );
    }
}

#[tokio::test]
async fn concurrency_above_one_actually_overlaps() {
    // Without this the limit could be honored by never running anything
    // concurrently at all, which would make the feature a no-op.
    let providers = many_dives_registry();
    let mut config = concurrent_config(4);
    config.limits.deep_calls = 4;
    crate::pipeline::run(&providers, &config, &input())
        .await
        .unwrap();
    assert!(
        recorded(&providers.deep).peak_in_flight() > 1,
        "dives must actually overlap"
    );
}

#[tokio::test]
async fn stages_stay_ordered() {
    let providers = many_dives_registry();
    let mut config = concurrent_config(4);
    config.limits.deep_calls = 4;
    config.profile = Some(crate::config::Profile::Deep);
    crate::pipeline::run(&providers, &config, &input())
        .await
        .unwrap();
    // Triage is asked exactly once, and its answer is what decides which
    // dives exist, so the dives cannot precede it.
    assert_eq!(recorded(&providers.triage).requests().len(), 1);
    let deep = recorded(&providers.deep);
    let names: Vec<String> = deep
        .requests()
        .iter()
        .map(|request| request.schema_name.clone())
        .collect();
    assert_eq!(
        names.last().map(String::as_str),
        Some("cross-examination"),
        "cross-examination runs only after every deep dive: {names:?}"
    );
    assert_eq!(
        names.iter().filter(|n| n.as_str() == "deep dive").count(),
        4
    );
}
