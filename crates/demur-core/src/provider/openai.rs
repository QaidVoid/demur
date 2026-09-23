//! Client for any OpenAI-compatible endpoint.

use std::collections::BTreeMap;
use std::time::Duration;

use super::redact;
use super::{CompletionRequest, CompletionResponse, Provider, ProviderError, TokenUsage};
use serde::Deserialize;
use serde_json::{Value, json};

/// Talks to any endpoint implementing the OpenAI chat completions dialect,
/// including gateways and self-hosted runtimes.
pub struct OpenAiClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    key: String,
    reasoning_effort: Option<String>,
    extra_body: Option<BTreeMap<String, Value>>,
    extra_headers: Option<BTreeMap<String, String>>,
}

impl OpenAiClient {
    /// Build a client from provider and model settings plus the resolved key.
    pub fn new(
        provider: &crate::config::ProviderDef,
        model: &crate::config::ModelDef,
        key: String,
    ) -> Result<Self, ProviderError> {
        let extra_body = super::extra_body_json(&provider.extra_body)?;
        Ok(OpenAiClient {
            http: http_client(),
            base_url: provider.base_url.trim_end_matches('/').to_string(),
            model: model.name.clone(),
            key,
            reasoning_effort: model.reasoning_effort.clone(),
            extra_body,
            extra_headers: provider.extra_headers.clone(),
        })
    }

    /// The URL completions are posted to.
    fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    fn body(&self, request: &CompletionRequest) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": request.system},
                {"role": "user", "content": request.user},
            ],
            "max_tokens": request.max_output_tokens,
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": request.schema_name,
                    "strict": true,
                    "schema": request.schema,
                },
            },
        });
        let object = body.as_object_mut().expect("body is an object");
        if let Some(effort) = &self.reasoning_effort {
            object.insert("reasoning_effort".into(), json!(effort));
        }
        if let Some(extra) = &self.extra_body {
            for (field, value) in extra {
                object.insert(field.clone(), value.clone());
            }
        }
        body
    }
}

impl Provider for OpenAiClient {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let body = self.body(request);
        let mut call = self
            .http
            .post(self.url())
            .bearer_auth(&self.key)
            .json(&body);
        if let Some(headers) = &self.extra_headers {
            for (name, value) in headers {
                call = call.header(name, value);
            }
        }
        let response = call.send().await.map_err(|err| ProviderError::Request {
            message: redact(&err.to_string(), &self.key),
        })?;
        let status = response.status();
        let retry_after = parse_retry_after(response.headers());
        let text = response
            .text()
            .await
            .map_err(|err| ProviderError::Request {
                message: redact(&err.to_string(), &self.key),
            })?;
        if !status.is_success() {
            return Err(super::with_request_excerpt(
                classify(status.as_u16(), &text, retry_after, &self.key),
                request,
            ));
        }
        let parsed: WireResponse =
            serde_json::from_str(&text).map_err(|err| ProviderError::Malformed {
                message: redact(&format!("body is not a chat completion: {err}"), &self.key),
                usage: TokenUsage::default(),
            })?;
        let finish_reason = parsed
            .choices
            .first()
            .and_then(|choice| choice.finish_reason.clone())
            .unwrap_or_default();
        let content = parsed
            .choices
            .first()
            .and_then(|choice| choice.message.content.clone())
            .unwrap_or_default();
        let usage = wire_usage(parsed.usage);
        if content.trim().is_empty() {
            return Err(truncated_or_malformed(
                &format!(
                    "empty message content: finish_reason={finish_reason}; raw response excerpt: {}",
                    body_excerpt(&text)
                ),
                &finish_reason,
                usage,
                &text,
                &self.key,
            ));
        }
        let content = match parse_json_content(&content, usage) {
            Ok(value) => value,
            Err(err) => {
                if finish_reason == "length" {
                    return Err(truncated_or_malformed(
                        &format!("{err}; the ceiling cut the JSON mid-output"),
                        &finish_reason,
                        usage,
                        &text,
                        &self.key,
                    ));
                }
                return Err(err);
            }
        };
        Ok(CompletionResponse {
            content,
            usage,
            reported_cost: None,
            reported_model: None,
        })
    }
}

pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .expect("static client configuration")
}

pub(crate) fn classify(
    status: u16,
    body: &str,
    retry_after: Option<Duration>,
    key: &str,
) -> ProviderError {
    let detail = redact(&body_excerpt(body), key);
    match status {
        401 | 403 => ProviderError::Auth { message: detail },
        429 => ProviderError::RateLimit {
            message: detail,
            retry_after,
        },
        400 if body.contains("context length")
            || body.contains("maximum context")
            || body.contains("too many tokens") =>
        {
            ProviderError::ContextOverflow { message: detail }
        }
        500..=599 => ProviderError::Request { message: detail },
        _ => ProviderError::Rejected { message: detail },
    }
}

pub(crate) fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: u64 = value.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

pub(crate) fn body_excerpt(body: &str) -> String {
    let excerpt: String = body.chars().take(400).collect();
    if body.chars().count() > 400 {
        format!("{excerpt}...")
    } else {
        excerpt
    }
}

/// Parse the model's text answer as JSON, tolerating markdown fencing.
/// A violation is billed: the usage belongs to the attempt that produced
/// the unusable answer.
pub(crate) fn parse_json_content(text: &str, usage: TokenUsage) -> Result<Value, ProviderError> {
    let trimmed = text.trim();
    let stripped = if trimmed.starts_with("```") {
        let without_fence = trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim();
        if !without_fence.starts_with('{') {
            match without_fence.find('{') {
                Some(start) => &without_fence[start..],
                None => trimmed,
            }
        } else {
            without_fence
        }
    } else {
        trimmed
    };
    serde_json::from_str(stripped).map_err(|err| ProviderError::Malformed {
        message: format!("content is not a JSON object matching the schema: {err}"),
        usage,
    })
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    choices: Vec<Choice>,
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: Message,
    finish_reason: Option<String>,
}

/// Convert the wire usage split, separating cached input from full-price
/// input.
fn wire_usage(usage: Option<WireUsage>) -> TokenUsage {
    let usage = usage.unwrap_or_default();
    let cached = usage
        .prompt_tokens_details
        .and_then(|details| details.cached_tokens)
        .unwrap_or(0);
    TokenUsage {
        input_tokens: usage.prompt_tokens.saturating_sub(cached),
        cached_input_tokens: cached,
        output_tokens: usage.completion_tokens,
    }
}

/// OutputTruncated when the finish reason says the ceiling was hit,
/// Malformed otherwise, carrying usage and a redacted excerpt either way.
fn truncated_or_malformed(
    detail: &str,
    finish_reason: &str,
    usage: TokenUsage,
    raw_body: &str,
    key: &str,
) -> ProviderError {
    let message = redact(
        &format!("{detail}; raw response excerpt: {}", body_excerpt(raw_body)),
        key,
    );
    if finish_reason == "length" {
        ProviderError::OutputTruncated { message, usage }
    } else {
        ProviderError::Malformed { message, usage }
    }
}

#[derive(Debug, Deserialize)]
struct Message {
    content: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct WireUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Debug, Deserialize)]
struct PromptDetails {
    cached_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Family, ModelDef, ProviderDef};
    use crate::provider::{RetryPolicy, complete_with_retries};
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn client(server: &MockServer) -> OpenAiClient {
        let provider = ProviderDef {
            family: Family::OpenAi,
            base_url: server.uri(),
            key_env: "TEST_KEY_ENV".to_string(),
            key_file: None,
            extra_body: toml::from_str("top_p = 0.5").unwrap(),
            extra_headers: Some(
                [("X-Custom", "custom-value")]
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
        };
        let model = ModelDef {
            provider: "test".to_string(),
            name: "gpt-test".to_string(),
            input_price: 1.0,
            output_price: 2.0,
            reasoning_effort: Some("high".to_string()),
            thinking_budget: None,
            extra_body: None,
            extra_headers: None,
            cached_input_price: None,
        };
        OpenAiClient::new(&provider, &model, "sk-test-key".to_string()).unwrap()
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

    fn success_body() -> Value {
        serde_json::json!({
            "choices": [{"message": {"content": "{\"verdict\": \"ok\"}"}}],
            "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 50,
                "prompt_tokens_details": {"cached_tokens": 700}
            }
        })
    }

    fn fast_policy() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 4,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
    }

    /// Replies with each template in order, repeating the last one.
    struct Seq(Vec<ResponseTemplate>, AtomicUsize);

    impl Respond for Seq {
        fn respond(&self, _request: &Request) -> ResponseTemplate {
            let index = self.1.fetch_add(1, Ordering::SeqCst);
            self.0[index.min(self.0.len() - 1)].clone()
        }
    }

    #[tokio::test]
    async fn request_carries_expected_shape() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("Authorization", "Bearer sk-test-key"))
            .and(header("X-Custom", "custom-value"))
            .and(body_partial_json(serde_json::json!({
                "model": "gpt-test",
                "messages": [
                    {"role": "system", "content": "stable context"},
                    {"role": "user", "content": "volatile data"}
                ],
                "reasoning_effort": "high",
                "top_p": 0.5,
                "response_format": {
                    "type": "json_schema",
                    "json_schema": {"name": "test_output", "strict": true}
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
            .expect(1)
            .mount(&server)
            .await;
        let response = complete_with_retries(&client(&server), &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(response.content, serde_json::json!({"verdict": "ok"}));
        server.verify().await;
    }

    #[tokio::test]
    async fn cached_usage_is_reported_separately() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(success_body()))
            .mount(&server)
            .await;
        let response = complete_with_retries(&client(&server), &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(response.usage.input_tokens, 300);
        assert_eq!(response.usage.cached_input_tokens, 700);
        assert_eq!(response.usage.output_tokens, 50);
    }

    #[tokio::test]
    async fn runs_against_provider_without_caching() {
        let server = MockServer::start().await;
        let plain = serde_json::json!({
            "choices": [{"message": {"content": "{\"a\": 1}"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(plain))
            .mount(&server)
            .await;
        let response = complete_with_retries(&client(&server), &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(
            response.usage,
            TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 0,
                output_tokens: 5,
            }
        );
    }

    #[tokio::test]
    async fn auth_failure_fails_without_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid key"))
            .expect(1)
            .mount(&server)
            .await;
        let err = complete_with_retries(&client(&server), &request(), &fast_policy())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Auth { .. }));
        server.verify().await;
    }

    #[tokio::test]
    async fn rate_limit_honors_hint_then_succeeds() {
        let server = MockServer::start().await;
        let limited = ResponseTemplate::new(429)
            .insert_header("Retry-After", "1")
            .set_body_string("slow down");
        let responder = Seq(
            vec![
                limited,
                ResponseTemplate::new(200).set_body_json(success_body()),
            ],
            AtomicUsize::new(0),
        );
        Mock::given(method("POST"))
            .respond_with(responder)
            .mount(&server)
            .await;
        let started = std::time::Instant::now();
        complete_with_retries(&client(&server), &request(), &fast_policy())
            .await
            .unwrap();
        assert!(started.elapsed() >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn context_overflow_is_classified() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("This model's maximum context length is 8192 tokens"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = complete_with_retries(&client(&server), &request(), &fast_policy())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::ContextOverflow { .. }));
        server.verify().await;
    }

    #[tokio::test]
    async fn key_material_is_redacted_from_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string("bad key sk-test-key for user"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = complete_with_retries(&client(&server), &request(), &fast_policy())
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("sk-test-key"));
        server.verify().await;
    }

    #[tokio::test]
    async fn fenced_json_content_parses() {
        let server = MockServer::start().await;
        let fenced = serde_json::json!({
            "choices": [{"message": {"content": "```json\n{\"a\": 1}\n```"}}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        });
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fenced))
            .mount(&server)
            .await;
        let response = complete_with_retries(&client(&server), &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(response.content, serde_json::json!({"a": 1}));
    }

    #[test]
    fn parse_json_content_rejects_prose_and_bills_it() {
        let err = parse_json_content(
            "here is my answer, not json",
            TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 0,
                output_tokens: 7,
            },
        )
        .unwrap_err();
        match err {
            ProviderError::Malformed { usage, .. } => {
                assert_eq!(
                    usage,
                    TokenUsage {
                        input_tokens: 100,
                        cached_input_tokens: 0,
                        output_tokens: 7
                    }
                );
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }
}
