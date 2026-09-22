//! Client that drives a local headless Claude Code installation as a
//! subprocess. The child holds its own login, so this path resolves no
//! key and makes no network connection of its own.

use std::process::Stdio;
use std::time::Duration;

use super::{CompletionRequest, CompletionResponse, Provider, ProviderError, TokenUsage};
use serde::Deserialize;
use tokio::process::Command;

/// The bot's fixed command. Never taken from configuration and never
/// taken from model output.
const CLAUDE_BIN: &str = "claude";

/// Wall-clock bound on one headless pass, after which the child is killed
/// and the pass fails as a request error.
const PASS_TIMEOUT: Duration = Duration::from_secs(600);

/// How much of the child's stderr a failure message may carry.
const STDERR_TAIL_CHARS: usize = 400;

/// Runs one pass through one headless `claude` process: prompt on standard
/// input, one JSON document out.
pub struct ClaudeCodeClient {
    model: String,
    binary: std::path::PathBuf,
    #[cfg(test)]
    timeout: Duration,
}

impl ClaudeCodeClient {
    /// Build a client for the configured model name. The command is the
    /// bot's own fixed constant, never configuration.
    pub fn new(model: &crate::config::ModelDef) -> Self {
        ClaudeCodeClient {
            model: model.name.clone(),
            binary: std::path::PathBuf::from(CLAUDE_BIN),
            #[cfg(test)]
            timeout: PASS_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn for_tests(binary: std::path::PathBuf, timeout: Duration) -> Self {
        ClaudeCodeClient {
            model: "test-model".to_string(),
            binary,
            timeout,
        }
    }
}

impl Provider for ClaudeCodeClient {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut child = Command::new(&self.binary)
            .arg("-p")
            .arg("--output-format")
            .arg("json")
            .arg("--json-schema")
            .arg(request.schema.to_string())
            .arg("--append-system-prompt")
            .arg(&request.system)
            .arg("--model")
            .arg(&self.model)
            .arg("--max-turns")
            .arg("1")
            .arg("--tools")
            .arg("")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| ProviderError::Request {
                message: format!("could not start {CLAUDE_BIN}: {err}"),
            })?;

        let user = request.user.clone();
        #[cfg(test)]
        let timeout = self.timeout;
        #[cfg(not(test))]
        let timeout = PASS_TIMEOUT;
        let output = tokio::time::timeout(timeout, async move {
            use tokio::io::AsyncWriteExt;
            let mut stdin = child.stdin.take().expect("child stdin was piped");
            stdin
                .write_all(user.as_bytes())
                .await
                .map_err(|err| format!("writing the prompt failed: {err}"))?;
            stdin
                .flush()
                .await
                .map_err(|err| format!("writing the prompt failed: {err}"))?;
            drop(stdin);
            child
                .wait_with_output()
                .await
                .map_err(|err| format!("waiting for the answer failed: {err}"))
        })
        .await;

        let output = match output {
            Ok(Ok(output)) => output,
            Ok(Err(message)) => return Err(ProviderError::Request { message }),
            Err(_) => {
                return Err(ProviderError::Request {
                    message: format!(
                        "the headless pass exceeded its {} second bound and was stopped",
                        timeout.as_secs()
                    ),
                });
            }
        };

        if !output.status.success() {
            return Err(ProviderError::Request {
                message: format!(
                    "{CLAUDE_BIN} exited with {}: {}",
                    output.status,
                    stderr_tail(&output.stderr)
                ),
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: CliResult =
            serde_json::from_str(stdout.trim()).map_err(|err| ProviderError::Request {
                message: format!(
                    "answer is not the expected JSON document: {err}; output excerpt: {}",
                    super::excerpt(&stdout)
                ),
            })?;
        if parsed.is_error.unwrap_or(false) {
            return Err(ProviderError::Request {
                message: format!(
                    "{CLAUDE_BIN} reported a failed turn: {}",
                    parsed.result.unwrap_or_default()
                ),
            });
        }
        let result = parsed.result.unwrap_or_default();
        let content = super::openai::parse_json_content(&result)?;
        let usage = parsed.usage.unwrap_or_default();
        Ok(CompletionResponse {
            content,
            usage: TokenUsage {
                input_tokens: usage.input_tokens + usage.cache_creation_input_tokens,
                cached_input_tokens: usage.cache_read_input_tokens,
                output_tokens: usage.output_tokens,
            },
        })
    }
}

fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(STDERR_TAIL_CHARS);
    chars[start..].iter().collect()
}

/// The fields demur reads from the CLI's `--output-format json` document.
/// Everything else is ignored, so unrelated CLI additions never break a
/// pass.
#[derive(Debug, Deserialize)]
struct CliResult {
    result: Option<String>,
    is_error: Option<bool>,
    usage: Option<CliUsage>,
}

#[derive(Debug, Deserialize, Default)]
struct CliUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{RetryPolicy, complete_with_retries};
    use std::path::PathBuf;

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "stable context".to_string(),
            user: "volatile data".to_string(),
            schema: serde_json::json!({"type": "object"}),
            schema_name: "test_output".to_string(),
            max_output_tokens: 100,
        }
    }

    /// A fixture script standing in for the CLI. It records its arguments
    /// and standard input beside itself, then answers per `behavior`.
    fn fixture(name: &str, body: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("demur-claude-fixture-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\nprintf '%s\\n' \"$*\" > {dir:?}/args\ncat > {dir:?}/stdin\n{body}\n"
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        (script, dir)
    }

    fn result_document(result: &str) -> String {
        serde_json::json!({
            "result": result,
            "is_error": false,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_input_tokens": 7,
                "cache_creation_input_tokens": 3
            }
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_clean_answer_parses_with_its_usage() {
        let (script, dir) = fixture(
            "clean",
            &format!("printf '%s' {}", sh_quote(&result_document("{\"a\": 1}"))),
        );
        let client = ClaudeCodeClient::for_tests(script, PASS_TIMEOUT);
        let response = complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(response.content, serde_json::json!({"a": 1}));
        assert_eq!(
            response.usage,
            TokenUsage {
                input_tokens: 13,
                cached_input_tokens: 7,
                output_tokens: 5,
            }
        );
        let args = std::fs::read_to_string(dir.join("args")).unwrap();
        assert!(args.contains("--output-format json"), "{args}");
        assert!(args.contains("--json-schema"), "{args}");
        assert!(args.contains("--model test-model"), "{args}");
        assert!(args.contains("--max-turns 1"), "{args}");
        assert!(args.contains("--tools"), "{args}");
        // The prompt travels on standard input, never the command line.
        assert!(
            !args.contains("volatile data"),
            "the prompt leaked onto the command line: {args}"
        );
        let stdin = std::fs::read_to_string(dir.join("stdin")).unwrap();
        assert_eq!(stdin, "volatile data");
    }

    #[tokio::test]
    async fn a_schema_invalid_answer_is_retried_into_shape() {
        let dir = std::env::temp_dir().join("demur-claude-fixture-corrected");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("claude");
        // First call answers in prose, the second in the expected shape.
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\ncat > /dev/null\nif [ -f {dir:?}/called ]; then\n  printf '%s' {}\nelse\n  touch {dir:?}/called\n  printf '%s' 'I cannot produce JSON today'\nfi\n",
                sh_quote(&result_document("{\"a\": 1}"))
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        let client = ClaudeCodeClient::for_tests(script, PASS_TIMEOUT);
        let response = complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(response.content, serde_json::json!({"a": 1}));
    }

    #[tokio::test]
    async fn a_stalled_child_hits_the_wall_clock_bound() {
        let (script, _dir) = fixture("slow", "sleep 30\n");
        let client = ClaudeCodeClient::for_tests(script, Duration::from_millis(200));
        let err = complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ProviderError::Request { .. }),
            "timeout surfaces as a request error: {err}"
        );
        assert!(err.to_string().contains("exceeded its"));
    }

    #[tokio::test]
    async fn a_nonzero_exit_carries_the_stderr_tail() {
        let (script, _dir) = fixture("failing", "echo 'provider auth expired' >&2\nexit 1\n");
        let client = ClaudeCodeClient::for_tests(script, PASS_TIMEOUT);
        let err = complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("exited with"), "{text}");
        assert!(text.contains("provider auth expired"), "{text}");
    }

    #[tokio::test]
    async fn an_error_turn_fails_as_a_request() {
        let document = serde_json::json!({
            "result": "the model refused",
            "is_error": true
        })
        .to_string();
        let (script, _dir) = fixture("errored", &format!("printf '%s' {}", sh_quote(&document)));
        let client = ClaudeCodeClient::for_tests(script, PASS_TIMEOUT);
        let err = complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Request { .. }), "{err}");
        assert!(err.to_string().contains("reported a failed turn"));
    }

    fn sh_quote(text: &str) -> String {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}
