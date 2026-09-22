//! End-to-end action binary tests: the fork path, draft skipping, and a
//! full review run against mocked GitHub and provider endpoints.

use std::fs;
use std::process::Command;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN_ENV: &str = "GITHUB_TOKEN";
const KEY_ENV: &str = "DEMUR_ACTION_TEST_KEY";
const KEY_VALUE: &str = "sk-action-test-key";

/// Build the job environment in a temp dir and run the action binary.
struct Job {
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    summary_path: std::path::PathBuf,
}

impl Job {
    fn new(name: &str, event: serde_json::Value, base_url: &str) -> (Job, Vec<std::ffi::OsString>) {
        let dir = tempfile::tempdir().unwrap();
        let event_path = dir.path().join("event.json");
        fs::write(&event_path, serde_json::to_string(&event).unwrap()).unwrap();
        let summary_path = dir.path().join("step-summary.md");
        let config = format!(
            r#"
[providers.openai]
family = "openai"
base_url = "{base_url}"
key_env = "{KEY_ENV}"

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
        );
        fs::write(dir.path().join(".demur.toml"), config).unwrap();
        let envs = vec![
            std::ffi::OsString::from(format!("GITHUB_EVENT_PATH={}", event_path.display())),
            std::ffi::OsString::from("GITHUB_REPOSITORY=owner/repo"),
            std::ffi::OsString::from(format!("GITHUB_STEP_SUMMARY={}", summary_path.display())),
            std::ffi::OsString::from(format!("GITHUB_WORKSPACE={}", dir.path().display())),
        ];
        let _ = name;
        (Job { dir, summary_path }, envs)
    }
}

fn run_binary(extra_envs: &[std::ffi::OsString], api_url: &str) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_demur-action"));
    command
        .env(TOKEN_ENV, "ghs-job-token")
        .env_remove(KEY_ENV)
        .env("GITHUB_API_URL", api_url);
    for env in extra_envs {
        let pair = env.to_string_lossy().to_string();
        let (name, value) = pair.split_once('=').unwrap();
        command.env(name, value);
    }
    command.output().unwrap()
}

fn event(action: &str, draft: bool, fork: bool) -> serde_json::Value {
    serde_json::json!({
        "action": action,
        "pull_request": {
            "number": 7,
            "draft": draft,
            "head": {"sha": "headsha", "repo": {"fork": fork}}
        }
    })
}

#[tokio::test]
async fn draft_pull_requests_skip_without_any_api_call() {
    let github = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
        .mount(&github)
        .await;
    let (job, envs) = Job::new("draft", event("opened", true, false), &github.uri());
    let output = run_binary(&envs, &github.uri());
    assert_eq!(output.status.code(), Some(0));
    github.verify().await;
    let _ = job;
}

#[tokio::test]
async fn fork_without_key_writes_notice_and_exits_success_without_api_calls() {
    let github = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(0)
        .mount(&github)
        .await;
    let (job, envs) = Job::new("fork", event("opened", false, true), &github.uri());
    let output = run_binary(&envs, &github.uri());
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = fs::read_to_string(&job.summary_path).unwrap();
    assert!(summary.contains("review skipped"));
    assert!(summary.contains("fork"));
    github.verify().await;
}

#[tokio::test]
async fn missing_key_on_a_non_fork_fails_with_setup_guidance() {
    let github = MockServer::start().await;
    let (_job, envs) = Job::new("nokey", event("opened", false, false), &github.uri());
    let output = run_binary(&envs, &github.uri());
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(KEY_ENV), "stderr: {stderr}");
    assert!(!stderr.contains(KEY_VALUE));
}

#[tokio::test]
async fn full_review_run_publishes_review_and_check_run() {
    let github = MockServer::start().await;
    let provider = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7"))
        .and(header("accept", "application/vnd.github+json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "number": 7, "draft": false, "head": {"sha": "headsha"}
        })))
        .mount(&github)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
        .mount(&github)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/pulls/7"))
        .and(header("accept", "application/vnd.github.v3.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "diff --git a/src.rs b/src.rs\n--- a/src.rs\n+++ b/src.rs\n@@ -1,2 +1,3 @@\n fn one() {\n+    let dangerous = true;\n }",
        ))
        .mount(&github)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": []}}}}
        })))
        .mount(&github)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/pulls/7/reviews"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 1})))
        .mount(&github)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/owner/repo/check-runs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .mount(&github)
        .await;

    // Provider responses: triage with a finding, a deep dive, then the
    // summary, each wrapped in the chat completion envelope.
    let steps = vec![
        serde_json::json!({
            "findings": [{
                "file": "src.rs",
                "start_line": 2,
                "end_line": 2,
                "severity": "warning",
                "message": "unbounded danger flag",
                "harm": "Merging lets any caller trip the dangerous path."
            }],
            "cluster_lens": [{"path": "src.rs", "lenses": []}]
        }),
        serde_json::json!({"summary": "The danger flag is unbounded."}),
    ];
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ProviderSeq(steps, std::sync::atomic::AtomicUsize::new(0)))
        .mount(&provider)
        .await;

    let (job, envs) = Job::new("full", event("opened", false, false), &provider.uri());
    let envs = set_env(envs, KEY_ENV, KEY_VALUE);
    let output = run_binary(&envs, &github.uri());
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary_text = fs::read_to_string(&job.summary_path).unwrap();
    assert!(summary_text.contains("unbounded danger flag"));
    github.verify().await;
}

/// Replies with each recorded response in order, repeating the last one,
/// wrapped in the OpenAI chat completion envelope.
struct ProviderSeq(Vec<serde_json::Value>, std::sync::atomic::AtomicUsize);

impl wiremock::Respond for ProviderSeq {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        use std::sync::atomic::Ordering;
        let index = self.1.fetch_add(1, Ordering::SeqCst);
        let index = index.min(self.0.len() - 1);
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": self.0[index].to_string()}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 20}
        }))
    }
}

fn set_env(envs: Vec<std::ffi::OsString>, name: &str, value: &str) -> Vec<std::ffi::OsString> {
    let mut envs = envs;
    envs.push(std::ffi::OsString::from(format!("{name}={value}")));
    envs
}
