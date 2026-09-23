//! Client that drives a local headless Claude Code installation as a
//! subprocess. The child holds its own login, so this path resolves no
//! key and makes no network connection of its own.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use super::{CompletionRequest, CompletionResponse, Provider, ProviderError, TokenUsage};
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;

/// The bot's fixed command. Never taken from configuration and never
/// taken from model output.
const CLAUDE_BIN: &str = "claude";

/// Wall-clock bound on one headless pass, after which the child is killed
/// and the pass fails as a request error.
const PASS_TIMEOUT: Duration = Duration::from_secs(600);

/// How much of the child's stderr a failure message may carry.
const STDERR_TAIL_CHARS: usize = 400;

/// Environment variables the headless child may inherit. Everything else
/// is scrubbed: the working directory the run started in is untrusted
/// checkout data, and the child must not read configuration, hooks, or
/// secrets out of its environment.
const CHILD_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "LANG",
    "LC_ALL",
    "TZ",
    "TMPDIR",
    "TMP",
    "TEMP",
    "CLAUDE_CONFIG_DIR",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
    "no_proxy",
];

fn child_env_allows(key: &str) -> bool {
    CHILD_ENV.contains(&key) || key.starts_with("XDG_")
}

/// Runs one pass through one headless `claude` process: prompt on standard
/// input, one JSON document out.
pub struct ClaudeCodeClient {
    model: String,
    effort: Option<String>,
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
            effort: model.effort.clone(),
            binary: std::path::PathBuf::from(CLAUDE_BIN),
            #[cfg(test)]
            timeout: PASS_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests(binary: std::path::PathBuf, timeout: Duration) -> Self {
        ClaudeCodeClient {
            model: "test-model".to_string(),
            effort: None,
            binary,
            timeout,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_effort(mut self, effort: &str) -> Self {
        self.effort = Some(effort.to_string());
        self
    }
}

impl Provider for ClaudeCodeClient {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        // The child never runs from the checkout: a pull request can ship
        // a `.claude` directory whose settings and hooks execute commands,
        // so the working directory is the neutral temporary directory and
        // the environment is the allowlist above.
        let spawn_error = |err: std::io::Error| ProviderError::Rejected {
            message: format!(
                "could not start {CLAUDE_BIN}: {err}\n\
install Claude Code (npm install -g @anthropic-ai/claude-code), make sure \
`{CLAUDE_BIN}` is on the PATH of this process, and run it once \
interactively to complete its login"
            ),
        };
        // A test fixture writes its stand-in moments before exec, and some
        // filesystems hold a fresh executable open for a beat, failing the
        // exec with Text file busy. The real binary is a stable
        // installation, so the retry exists for the test build alone.
        let build = || {
            let mut command = Command::new(&self.binary);
            command
                .arg("-p")
                .arg("--output-format")
                .arg("json")
                .arg("--json-schema")
                .arg(request.schema.to_string())
                .arg("--append-system-prompt")
                .arg(&request.system)
                .arg("--model")
                .arg(&self.model);
            if let Some(effort) = &self.effort {
                command.arg("--effort").arg(effort);
            }
            command
                .arg("--max-turns")
                .arg("1")
                .arg("--tools")
                .arg("")
                .env_clear();
            for (key, value) in std::env::vars_os() {
                if child_env_allows(&key.to_string_lossy()) {
                    command.env(key, value);
                }
            }
            command.current_dir(std::env::temp_dir());
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            command
        };
        let mut child = match build().spawn() {
            Ok(child) => child,
            Err(err) if cfg!(test) && err.raw_os_error() == Some(26) => {
                tokio::time::sleep(Duration::from_millis(25)).await;
                build().spawn().map_err(spawn_error)?
            }
            Err(err) => return Err(spawn_error(err)),
        };

        let user = request.user.clone();
        #[cfg(test)]
        let timeout = self.timeout;
        #[cfg(not(test))]
        let timeout = PASS_TIMEOUT;
        let stdin = child.stdin.take().expect("child stdin was piped");
        let stdin_task = tokio::spawn(write_prompt(stdin, user));
        let waited = tokio::time::timeout(timeout, child.wait_with_output()).await;

        let output = match waited {
            Ok(Ok(output)) => output,
            Ok(Err(err)) => {
                return Err(ProviderError::Request {
                    message: format!("waiting for the answer failed: {err}"),
                });
            }
            Err(_) => {
                return Err(ProviderError::Request {
                    message: format!(
                        "the headless pass exceeded its {} second bound and was stopped",
                        timeout.as_secs()
                    ),
                });
            }
        };
        // A child that exits early without reading the prompt breaks the
        // write. Its own exit status and stderr name the cause, so they
        // are reported first and the write failure only preempts an
        // otherwise successful answer, which a truncated prompt cannot
        // be trusted to produce.
        if !output.status.success() {
            return Err(ProviderError::Request {
                message: child_failure(&output.status, &output.stderr, &output.stdout),
            });
        }
        let wrote = match tokio::time::timeout(timeout, stdin_task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(err))) => Err(err),
            Ok(Err(join)) => Err(std::io::Error::other(join.to_string())),
            Err(_) => Err(std::io::Error::other("the prompt write did not finish")),
        };
        if let Err(err) = wrote {
            return Err(ProviderError::Request {
                message: format!("writing the prompt failed: {err}"),
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut parsed: CliResult =
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
        let reported_model = parsed.observed_model();
        let result = parsed.result.take().unwrap_or_default();
        let cli_usage = parsed.usage.take().unwrap_or_default();
        let usage = TokenUsage {
            input_tokens: cli_usage.input_tokens + cli_usage.cache_creation_input_tokens,
            cached_input_tokens: cli_usage.cache_read_input_tokens,
            output_tokens: cli_usage.output_tokens,
        };
        let content = super::openai::parse_json_content(&result, usage)?;
        Ok(CompletionResponse {
            content,
            usage,
            reported_cost: parsed.total_cost_usd,
            reported_model,
        })
    }
}

fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let chars: Vec<char> = text.chars().collect();
    let start = chars.len().saturating_sub(STDERR_TAIL_CHARS);
    chars[start..].iter().collect()
}

/// Name why the child failed. Stderr wins when it said anything; the
/// headless CLI otherwise reports its failures as JSON on standard output
/// (a missing login, a rate limit, an API error), and an empty detail
/// after the exit status names nothing. A missing login gets setup
/// guidance, because from the outside it looks like a broken install.
fn child_failure(status: &std::process::ExitStatus, stderr: &[u8], stdout: &[u8]) -> String {
    let tail = stderr_tail(stderr);
    let detail = if tail.trim().is_empty() {
        stdout_reason(stdout)
    } else {
        tail
    };
    let mut message = format!("{CLAUDE_BIN} exited with {status}: {detail}");
    let lowered = detail.to_lowercase();
    if lowered.contains("not logged in") || lowered.contains("/login") {
        message.push_str(
            "\nthe headless CLI holds no login. Run `claude` once interactively to log in; \
if your login lives under a CLAUDE_CONFIG_DIR, that variable must be set for \
the process that runs demur as well, since the child inherits only an \
allowlist of variables and this is one of them",
        );
    }
    message
}

/// Extract the failure reason from the child's JSON result document.
fn stdout_reason(stdout: &[u8]) -> String {
    let text = String::from_utf8_lossy(stdout);
    serde_json::from_str::<Value>(text.trim())
        .ok()
        .and_then(|document| {
            document
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or_else(|| "no reason reported on standard error or in its result document".into())
}

/// Feed the prompt while the answer is drained, so a child that stops
/// reading cannot wedge the pass against a full pipe.
async fn write_prompt(
    mut stdin: tokio::process::ChildStdin,
    prompt: String,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    stdin.write_all(prompt.as_bytes()).await?;
    stdin.flush().await?;
    stdin.shutdown().await
}

/// The fields demur reads from the CLI's `--output-format json` document.
/// Everything else is ignored, so unrelated CLI additions never break a
/// pass.
#[derive(Debug, Deserialize)]
struct CliResult {
    result: Option<String>,
    is_error: Option<bool>,
    usage: Option<CliUsage>,
    total_cost_usd: Option<f64>,
    #[serde(rename = "modelUsage")]
    model_usage: Option<BTreeMap<String, Value>>,
}

impl CliResult {
    /// The model that did the work: the CLI keys its model usage by the
    /// model id, and a fallback spends the most where it answered. A
    /// document without a positive spend figure names no model, because a
    /// free fallback entry is not evidence of what answered.
    fn observed_model(&self) -> Option<String> {
        let usage = self.model_usage.as_ref()?;
        let figure = |spent: &Value| {
            spent
                .get("costUSD")
                .or_else(|| spent.get("total_cost_usd"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        };
        let (model, spent) = usage
            .iter()
            .max_by_key(|(_, spent)| figure(spent).to_bits())?;
        (figure(spent) > 0.0).then(|| model.clone())
    }
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

    /// A fixture script standing in for the CLI. It records its arguments,
    /// standard input, working directory, and the test environment
    /// variables it received, then answers per `behavior`.
    fn fixture(name: &str, body: &str) -> (PathBuf, tempfile::TempDir) {
        let dir = tempfile::TempDir::with_prefix(format!("demur-claude-{name}-")).unwrap();
        let dir_path = dir.path();
        let script = dir_path.join("claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\nprintf '%s\\n' \"$*\" > {dir_path:?}/args\ncat > {dir_path:?}/stdin\npwd > {dir_path:?}/cwd\nenv | grep -E '^(DEMUR_TEST_[A-Z_]+|CLAUDE_CONFIG_DIR)=' > {dir_path:?}/childenv || true\n{body}\n"
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
    async fn the_answer_carries_the_reported_cost_and_model() {
        let document = serde_json::json!({
            "result": "{\"a\": 1}",
            "is_error": false,
            "total_cost_usd": 0.0123,
            "modelUsage": {
                "claude-sonnet-4-5-20260101": {"costUSD": 0.0123},
                "claude-haiku-4-5-20260101": {"costUSD": 0.0001}
            }
        })
        .to_string();
        let (script, _dir) = fixture("costed", &format!("printf '%s' {}", sh_quote(&document)));
        let client = ClaudeCodeClient::for_tests(script, PASS_TIMEOUT);
        let response = complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(response.reported_cost, Some(0.0123));
        assert_eq!(
            response.reported_model.as_deref(),
            Some("claude-sonnet-4-5-20260101")
        );
    }

    #[tokio::test]
    async fn a_missing_cost_figure_stays_none() {
        let (script, _dir) = fixture(
            "uncosted",
            &format!("printf '%s' {}", sh_quote(&result_document("{\"a\": 1}"))),
        );
        let client = ClaudeCodeClient::for_tests(script, PASS_TIMEOUT);
        let response = complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(response.reported_cost, None);
        assert_eq!(response.reported_model, None);
    }

    #[tokio::test]
    async fn a_zero_cost_model_usage_names_no_model() {
        let document = serde_json::json!({
            "result": "{\"a\": 1}",
            "is_error": false,
            "modelUsage": {"claude-sonnet-4-5-20260101": {"costUSD": 0.0}}
        })
        .to_string();
        let (script, _dir) = fixture("free", &format!("printf '%s' {}", sh_quote(&document)));
        let client = ClaudeCodeClient::for_tests(script, PASS_TIMEOUT);
        let response = complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(response.reported_cost, None);
        assert_eq!(response.reported_model, None);
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
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
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
        let stdin = std::fs::read_to_string(dir.path().join("stdin")).unwrap();
        assert_eq!(stdin, "volatile data");
    }

    #[tokio::test]
    async fn the_configured_effort_rides_the_command_line() {
        let (script, dir) = fixture(
            "effort",
            &format!("printf '%s' {}", sh_quote(&result_document("{\"a\": 1}"))),
        );
        let client = ClaudeCodeClient::for_tests(script, PASS_TIMEOUT).with_effort("xhigh");
        complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap();
        let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
        assert!(args.contains("--effort xhigh"), "{args}");
        assert!(args.contains("--model test-model --effort xhigh"), "{args}");
    }

    #[tokio::test]
    async fn the_child_runs_scrubbed_outside_the_checkout() {
        use std::env;
        unsafe { env::set_var("DEMUR_TEST_SENTINEL", "secret-from-the-checkout") };
        unsafe { env::set_var("CLAUDE_CONFIG_DIR", "/demur-test-claude-config") };
        let (script, dir) = fixture(
            "scrubbed",
            &format!("printf '%s' {}", sh_quote(&result_document("{\"a\": 1}"))),
        );
        let client = ClaudeCodeClient::for_tests(script, PASS_TIMEOUT);
        complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap();
        let childenv = std::fs::read_to_string(dir.path().join("childenv")).unwrap();
        assert!(
            !childenv.contains("DEMUR_TEST_SENTINEL"),
            "the child saw an unlisted variable: {childenv}"
        );
        assert!(
            childenv.contains("CLAUDE_CONFIG_DIR=/demur-test-claude-config"),
            "the child lost the config dir: {childenv}"
        );
        let cwd = std::fs::read_to_string(dir.path().join("cwd")).unwrap();
        let checkout = std::env::current_dir().unwrap();
        assert_ne!(
            PathBuf::from(cwd.trim()),
            checkout,
            "the child ran from the checkout: {cwd}"
        );
    }

    #[tokio::test]
    async fn a_schema_invalid_answer_is_retried_into_shape() {
        let dir = tempfile::TempDir::with_prefix("demur-claude-corrected-").unwrap();
        let dir_path = dir.path();
        let script = dir_path.join("claude");
        // First call answers in prose, the second in the expected shape.
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\ncat > /dev/null\nif [ -f {dir_path:?}/called ]; then\n  printf '%s' {}\nelse\n  touch {dir_path:?}/called\n  printf '%s' 'I cannot produce JSON today'\nfi\n",
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
    async fn a_silent_stderr_falls_back_to_the_result_document() {
        let document = serde_json::json!({
            "subtype": "success",
            "is_error": true,
            "terminal_reason": "api_error",
            "result": "rate limit reached, try again later"
        })
        .to_string();
        let (script, _dir) = fixture(
            "stdout-failure",
            &format!("printf '%s' {}\nexit 1\n", sh_quote(&document)),
        );
        let client = ClaudeCodeClient::for_tests(script, PASS_TIMEOUT);
        let err = complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("exited with"), "{text}");
        assert!(text.contains("rate limit reached"), "{text}");
    }

    #[tokio::test]
    async fn a_missing_login_gets_setup_guidance() {
        let document = serde_json::json!({
            "subtype": "success",
            "is_error": true,
            "result": "Not logged in · Please run /login"
        })
        .to_string();
        let (script, _dir) = fixture(
            "logged-out",
            &format!("printf '%s' {}\nexit 1\n", sh_quote(&document)),
        );
        let client = ClaudeCodeClient::for_tests(script, PASS_TIMEOUT);
        let err = complete_with_retries(&client, &request(), &fast_policy())
            .await
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("Not logged in"), "{text}");
        assert!(text.contains("interactively to log in"), "{text}");
        assert!(text.contains("CLAUDE_CONFIG_DIR"), "{text}");
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
