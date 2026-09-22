//! End-to-end CLI tests: local reviews against a mock provider, exit
//! statuses, and credential redaction.

use std::fs;
use std::path::Path;
use std::process::Command;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY_ENV: &str = "DEMUR_CLI_TEST_KEY";
const KEY_VALUE: &str = "sk-cli-test-key";

fn fixture_repo(_name: &str, server_uri: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    fs::write(repo.join("src.rs"), "fn one() {}\n").unwrap();
    for command in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec!["commit", "-q", "-m", "one"],
    ] {
        let status = Command::new("git")
            .args(&command)
            .current_dir(repo)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {command:?} failed");
    }
    fs::write(repo.join("src.rs"), "fn one() {}\nfn two() {}\n").unwrap();
    fs::write(
        repo.join(".demur.toml"),
        format!(
            r#"
[providers.openai]
family = "openai"
base_url = "{server_uri}"
key_env = "{KEY_ENV}"

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
        ),
    )
    .unwrap();
    dir
}

fn run_cli(repo: &Path, format: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_demur"))
        .args(["review"])
        .args(["--repo", repo.to_str().unwrap(), "--format", format])
        .env(KEY_ENV, KEY_VALUE)
        .output()
        .unwrap()
}

/// Wrap a pass response in the OpenAI chat completion envelope.
fn envelope(content: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "choices": [{"message": {"content": content.to_string()}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 20}
    })
}

/// Replies with each response in order, repeating the last one.
struct Seq(Vec<serde_json::Value>, std::sync::atomic::AtomicUsize);

impl wiremock::Respond for Seq {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        use std::sync::atomic::Ordering;
        let index = self.1.fetch_add(1, Ordering::SeqCst);
        let index = index.min(self.0.len() - 1);
        ResponseTemplate::new(200).set_body_json(envelope(self.0[index].clone()))
    }
}

async fn mount_responses(responses: Vec<serde_json::Value>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(Seq(responses, std::sync::atomic::AtomicUsize::new(0)))
        .mount(&server)
        .await;
    server
}

fn triage_response() -> serde_json::Value {
    serde_json::json!({
        "findings": [],
        "cluster_lens": [{"path": "src.rs", "lenses": []}]
    })
}

fn summary_response() -> serde_json::Value {
    serde_json::json!({"summary": "Nothing to argue against."})
}

#[tokio::test]
async fn approve_prints_json_and_exits_zero() {
    let server = mount_responses(vec![triage_response(), summary_response()]).await;
    let dir = fixture_repo("approve", &server.uri());
    let output = run_cli(dir.path(), "json");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(parsed["verdict"], "approve");
    assert!(!parsed["spend"]["passes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn blocking_finding_exits_one() {
    let server = mount_responses(vec![
        serde_json::json!({
            "findings": [{
                "file": "src.rs",
                "start_line": 2,
                "end_line": 2,
                "severity": "blocker",
                "message": "unbounded write",
                "harm": "Merging lets any caller overwrite the config file."
            }],
            "cluster_lens": [{"path": "src.rs", "lenses": []}]
        }),
        serde_json::json!({"summary": "The write is unbounded."}),
    ])
    .await;
    let _ = &server;
    let dir = fixture_repo("blocker", &server.uri());
    let output = run_cli(dir.path(), "markdown");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("changes requested"));
    assert!(stdout.contains("unbounded write"));
}

#[tokio::test]
async fn provider_failure_exits_two_with_redacted_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string(format!("bad key {KEY_VALUE}")))
        .mount(&server)
        .await;
    let dir = fixture_repo("authfail", &server.uri());
    let output = run_cli(dir.path(), "markdown");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains(KEY_VALUE), "key leaked: {stderr}");
    assert!(output.stdout.iter().all(|byte| byte.is_ascii_whitespace()));
}

#[tokio::test]
async fn missing_key_fails_with_setup_guidance() {
    let dir = fixture_repo("nokey", "https://127.0.0.1:1");
    let output = Command::new(env!("CARGO_BIN_EXE_demur"))
        .args(["review", "--repo", dir.path().to_str().unwrap()])
        .env_remove(KEY_ENV)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(KEY_ENV));
}

#[tokio::test]
async fn a_working_copy_review_refuses_a_cache_directory() {
    // A working copy has no commit to key entries to, and it changes
    // without recording that it did, so caching one could serve an answer
    // about code that is no longer there.
    let server = mount_responses(vec![]).await;
    let repo = fixture_repo("working-copy-cache", &server.uri());
    let cache = repo.path().join("cache");
    let output = Command::new(env!("CARGO_BIN_EXE_demur"))
        .args(["review"])
        .args(["--repo", repo.path().to_str().unwrap()])
        .args(["--cache-dir", cache.to_str().unwrap()])
        .env(KEY_ENV, KEY_VALUE)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("needs a revision range"),
        "expected a refusal naming the reason: {stderr}"
    );
    assert!(!cache.exists(), "the refused run creates no cache location");
}

#[tokio::test]
async fn a_local_review_without_the_flag_writes_no_cache() {
    let server = mount_responses(vec![triage_response(), summary_response()]).await;
    let repo = fixture_repo("no-cache", &server.uri());
    let before: Vec<_> = std::fs::read_dir(repo.path())
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .collect();
    run_cli(repo.path(), "json");
    let after: Vec<_> = std::fs::read_dir(repo.path())
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
        .collect();
    assert_eq!(
        before.len(),
        after.len(),
        "a run without --cache-dir leaves nothing behind"
    );
}
