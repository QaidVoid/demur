//! BYOK provider clients: the completion interface, error classification,
//! key resolution with redaction, and bounded retries.

mod anthropic;
mod openai;

pub use anthropic::AnthropicClient;
pub use openai::OpenAiClient;

/// First bytes of a provider response body, for error detail.
pub fn excerpt(body: &str) -> String {
    openai::body_excerpt(body)
}

use std::collections::BTreeMap;
use std::env;
use std::time::Duration;

use crate::config::{Config, Family, ModelDef, ProviderDef};
use serde_json::Value;
use thiserror::Error;

/// One completion call against a provider model.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// Stable context placed first and marked cacheable where supported.
    pub system: String,
    /// The volatile content: delimited pull request data and instructions.
    pub user: String,
    /// JSON schema the response must satisfy.
    pub schema: Value,
    /// Name for the schema, for providers that label structured output.
    pub schema_name: String,
    /// Maximum output tokens.
    pub max_output_tokens: u32,
}

/// Token usage reported by a provider, split by cached input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Input tokens billed at the full input price.
    pub input_tokens: u64,
    /// Input tokens served from cache and billed at the cached price.
    pub cached_input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
}

impl TokenUsage {
    /// Cost in USD at the given per-million-token prices.
    pub fn cost_usd(&self, input_price: f64, output_price: f64, cached_price: f64) -> f64 {
        let uncached = self.input_tokens as f64 / 1_000_000.0 * input_price;
        let cached = self.cached_input_tokens as f64 / 1_000_000.0 * cached_price;
        let output = self.output_tokens as f64 / 1_000_000.0 * output_price;
        uncached + cached + output
    }
}

/// A validated completion response with its usage.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    /// Parsed JSON content of the response.
    pub content: Value,
    /// Token usage reported for the call.
    pub usage: TokenUsage,
}

/// Classified provider failures. Variants carry redacted messages only.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The key is absent or was rejected. Fail fast with setup guidance.
    #[error(
        "provider key problem: {message}\nset the key in the environment variable named by key_env, or the file named by key_file, then rerun"
    )]
    Auth {
        /// Redacted detail about what went wrong.
        message: String,
    },
    /// The provider rate limited the call. Retry within bounded backoff.
    #[error("provider rate limited the request: {message}")]
    RateLimit {
        /// Redacted detail from the provider.
        message: String,
        /// Retry interval the provider hinted at, if any.
        retry_after: Option<Duration>,
    },
    /// The prompt exceeds the model context window. Shrink and retry.
    #[error("prompt exceeds the model context window: {message}")]
    ContextOverflow {
        /// Redacted detail from the provider.
        message: String,
    },
    /// The response failed validation. Retry within bounds, then fail.
    #[error("provider response did not satisfy the schema: {message}")]
    Malformed {
        /// Redacted detail about the violation.
        message: String,
    },
    /// The model spent its whole output budget without producing usable
    /// text. Retryable only with a raised ceiling.
    #[error("provider hit the output token ceiling: {message}")]
    OutputTruncated {
        /// Redacted detail, including the stop reason and an excerpt.
        message: String,
        /// Usage of the wasted call, so spend stays honest.
        usage: TokenUsage,
    },
    /// The provider rejected the request permanently. No retries help.
    #[error("provider rejected the request: {message}")]
    Rejected {
        /// Redacted detail from the provider.
        message: String,
    },
    /// Transport or server failure. Retryable within bounds.
    #[error("provider request failed: {message}")]
    Request {
        /// Redacted detail about the failure.
        message: String,
    },
}

/// True when a retry at the same output ceiling can still change the
/// outcome. Truncation is excluded: the same ceiling repeats the failure.
pub fn is_retryable(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::RateLimit { .. }
            | ProviderError::Malformed { .. }
            | ProviderError::Request { .. }
    )
}

/// Bounded retry behavior for provider calls.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Total attempts including the first.
    pub max_attempts: u32,
    /// Backoff base for exponential retries without a provider hint.
    pub base_delay: Duration,
    /// Upper bound on a computed backoff sleep.
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Anything that can complete a structured request.
pub trait Provider {
    /// Run one completion. Errors are classified and redacted.
    fn complete(
        &self,
        request: &CompletionRequest,
    ) -> impl std::future::Future<Output = Result<CompletionResponse, ProviderError>> + Send;
}

/// Run a completion with bounded retries for retryable failures.
pub async fn complete_with_retries<P: Provider>(
    provider: &P,
    request: &CompletionRequest,
    policy: &RetryPolicy,
) -> Result<CompletionResponse, ProviderError> {
    let mut attempt: u32 = 1;
    loop {
        match provider.complete(request).await {
            Ok(response) => return Ok(response),
            Err(ProviderError::RateLimit { retry_after, .. }) if attempt < policy.max_attempts => {
                let backoff = retry_after
                    .map(|hint| hint.min(Duration::from_secs(60)))
                    .unwrap_or_else(|| backoff_delay(policy, attempt));
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
            Err(err) if is_retryable(&err) && attempt < policy.max_attempts => {
                tokio::time::sleep(backoff_delay(policy, attempt)).await;
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

fn backoff_delay(policy: &RetryPolicy, attempt: u32) -> Duration {
    (policy.base_delay * 2u32.saturating_pow(attempt.saturating_sub(1))).min(policy.max_delay)
}

/// Providers bound to the configured model roles.
pub struct ProviderRegistry {
    /// Provider for the triage pass.
    pub triage: AnyProvider,
    /// Provider for deep dives and cross-examination.
    pub deep: AnyProvider,
    /// Provider for verdict synthesis.
    pub verdict: AnyProvider,
}

impl ProviderRegistry {
    /// Build a provider for every role, resolving each key from the
    /// environment or the configured key file.
    pub fn from_config(config: &Config) -> Result<ProviderRegistry, ProviderError> {
        Ok(ProviderRegistry {
            triage: build_role(&config.models.triage, config)?,
            deep: build_role(&config.models.deep, config)?,
            verdict: build_role(&config.models.verdict, config)?,
        })
    }

    /// Build a registry of recorded providers, for fixture harness runs.
    pub fn recorded(
        triage: crate::pipeline::RecordedProvider,
        deep: crate::pipeline::RecordedProvider,
        verdict: crate::pipeline::RecordedProvider,
    ) -> ProviderRegistry {
        ProviderRegistry {
            triage: AnyProvider::Recorded(triage),
            deep: AnyProvider::Recorded(deep),
            verdict: AnyProvider::Recorded(verdict),
        }
    }
}

fn build_role(model: &ModelDef, config: &Config) -> Result<AnyProvider, ProviderError> {
    let provider = config
        .providers
        .get(&model.provider)
        .ok_or_else(|| ProviderError::Rejected {
            message: format!(
                "model role names unknown provider `{}`; configuration validation should have caught this",
                model.provider
            ),
        })?;
    let key = resolve_key(provider)?;
    Ok(match provider.family {
        Family::OpenAi => AnyProvider::OpenAi(OpenAiClient::new(provider, model, key)?),
        Family::Anthropic => AnyProvider::Anthropic(AnthropicClient::new(provider, model, key)?),
    })
}

/// The concrete provider behind a role.
pub enum AnyProvider {
    /// An OpenAI-compatible endpoint.
    OpenAi(OpenAiClient),
    /// The native Anthropic API.
    Anthropic(AnthropicClient),
    /// Recorded responses, for the fixture harness and offline replay.
    Recorded(crate::pipeline::RecordedProvider),
}

impl Provider for AnyProvider {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        match self {
            AnyProvider::OpenAi(client) => client.complete(request).await,
            AnyProvider::Anthropic(client) => client.complete(request).await,
            AnyProvider::Recorded(client) => client.complete(request).await,
        }
    }
}

/// Read the key for a provider from its environment variable or key file.
pub fn resolve_key(provider: &ProviderDef) -> Result<String, ProviderError> {
    if let Ok(value) = env::var(&provider.key_env) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    if let Some(file) = &provider.key_file {
        // A named file that cannot be read is a configuration mistake worth
        // naming. Swallowing the error reports it as a missing key and
        // sends the user hunting the wrong variable.
        let value = std::fs::read_to_string(file).map_err(|err| ProviderError::Auth {
            message: format!("key_file `{}` could not be read: {err}", file.display()),
        })?;
        let trimmed = value.lines().next().unwrap_or_default().trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
        return Err(ProviderError::Auth {
            message: format!("key_file `{}` is empty", file.display()),
        });
    }
    Err(ProviderError::Auth {
        message: format!(
            "no provider key found for `{}`: set the {} environment variable",
            provider.base_url, provider.key_env
        ),
    })
}

/// Attach the shape of what was sent to a permanent rejection. A provider
/// or gateway that refuses a request refuses it because of the content,
/// and without an excerpt the only way to learn which content is to pay
/// for the whole run again.
pub(crate) fn with_request_excerpt(
    error: ProviderError,
    request: &CompletionRequest,
) -> ProviderError {
    let ProviderError::Rejected { message } = error else {
        return error;
    };
    let chars = request.user.chars().count();
    let head: String = request.user.chars().take(200).collect();
    let tail: String = request
        .user
        .chars()
        .skip(chars.saturating_sub(200))
        .collect();
    ProviderError::Rejected {
        message: format!(
            "{message}; the rejected request carried {chars} characters of content, \
starting `{head}` and ending `{tail}`"
        ),
    }
}

/// Replace any occurrence of the key in a message before it can reach logs
/// or publications.
pub fn redact(message: &str, secret: &str) -> String {
    if secret.is_empty() {
        message.to_string()
    } else {
        message.replace(secret, "[redacted]")
    }
}

/// Convert configured passthrough body fields to JSON.
pub(crate) fn extra_body_json(
    extra: &Option<toml::Table>,
    key: &str,
) -> Result<Option<BTreeMap<String, Value>>, ProviderError> {
    let Some(table) = extra else {
        return Ok(None);
    };
    let mut map = BTreeMap::new();
    for (field, value) in table {
        let json = serde_json::to_value(value).map_err(|err| ProviderError::Rejected {
            message: format!("extra_body.{field} is not representable in JSON: {err}"),
        })?;
        map.insert(field.clone(), json);
    }
    let _ = key;
    Ok(Some(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn usage_cost_splits_cached_and_uncached_input() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 500_000,
            output_tokens: 100_000,
        };
        let cost = usage.cost_usd(3.0, 15.0, 0.3);
        let expected = 3.0 + 0.3 * 0.5 + 15.0 * 0.1;
        assert!((cost - expected).abs() < 1e-9);
    }

    #[test]
    fn auth_error_carries_setup_guidance() {
        let err = ProviderError::Auth {
            message: "rejected".to_string(),
        };
        let text = err.to_string();
        assert!(text.contains("key_env"));
    }

    #[test]
    fn redact_removes_key_material() {
        let redacted = redact("bad key sk-secret-value for user", "sk-secret-value");
        assert!(!redacted.contains("sk-secret-value"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn resolve_key_reads_the_environment_variable() {
        let def = provider_def(None);
        unsafe { env::set_var("DEMUR_TEST_KEY_VAR", " sk-from-env ") };
        assert_eq!(resolve_key(&def).unwrap(), "sk-from-env");
    }

    #[test]
    fn resolve_key_reads_the_key_file() {
        let file = std::env::temp_dir().join("demur-test-key-file-8153");
        std::fs::write(&file, "sk-from-file\n").unwrap();
        let mut def = provider_def(Some(file));
        def.key_env = "DEMUR_TEST_FILE_FALLBACK_VAR".to_string();
        unsafe { env::remove_var("DEMUR_TEST_FILE_FALLBACK_VAR") };
        assert_eq!(resolve_key(&def).unwrap(), "sk-from-file");
    }

    #[test]
    fn missing_key_names_the_variable() {
        let mut def = provider_def(None);
        def.key_env = "DEMUR_TEST_MISSING_KEY_VAR".to_string();
        unsafe { env::remove_var("DEMUR_TEST_MISSING_KEY_VAR") };
        let err = resolve_key(&def).unwrap_err();
        assert!(err.to_string().contains("DEMUR_TEST_MISSING_KEY_VAR"));
    }

    fn provider_def(key_file: Option<PathBuf>) -> ProviderDef {
        ProviderDef {
            family: crate::config::Family::OpenAi,
            base_url: "https://api.example.com/v1".to_string(),
            key_env: "DEMUR_TEST_KEY_VAR".to_string(),
            key_file,
            extra_body: None,
            extra_headers: None,
        }
    }

    #[test]
    fn retryable_classification() {
        assert!(is_retryable(&ProviderError::RateLimit {
            message: String::new(),
            retry_after: None,
        }));
        assert!(is_retryable(&ProviderError::Request {
            message: String::new()
        }));
        assert!(!is_retryable(&ProviderError::Auth {
            message: String::new()
        }));
        assert!(!is_retryable(&ProviderError::ContextOverflow {
            message: String::new()
        }));
    }
}
