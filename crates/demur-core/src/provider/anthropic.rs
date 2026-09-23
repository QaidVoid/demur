//! Client for the native Anthropic messages API.

use std::collections::BTreeMap;

use super::openai::{body_excerpt, classify, http_client, parse_json_content};
use super::redact;
use super::{CompletionRequest, CompletionResponse, Provider, ProviderError, TokenUsage};
use serde::Deserialize;
use serde_json::{Value, json};

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Talks to the native Anthropic messages API with thinking budget
/// translation and prompt caching via cache control markers.
pub struct AnthropicClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    key: String,
    thinking_budget: Option<u32>,
    extra_body: Option<BTreeMap<String, Value>>,
    extra_headers: Option<BTreeMap<String, String>>,
}

impl AnthropicClient {
    /// Build a client from provider and model settings plus the resolved key.
    pub fn new(
        provider: &crate::config::ProviderDef,
        model: &crate::config::ModelDef,
        key: String,
    ) -> Result<Self, ProviderError> {
        let extra_body = super::extra_body_json(&provider.extra_body)?;
        Ok(AnthropicClient {
            http: http_client(),
            base_url: provider.base_url.trim_end_matches('/').to_string(),
            model: model.name.clone(),
            key,
            thinking_budget: model.thinking_budget,
            extra_body,
            extra_headers: provider.extra_headers.clone(),
        })
    }

    fn body(&self, request: &CompletionRequest) -> Value {
        let max_tokens = match self.thinking_budget {
            Some(budget) => budget.saturating_add(request.max_output_tokens),
            None => request.max_output_tokens,
        };
        let mut body = json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": [{
                "type": "text",
                "text": request.system,
                "cache_control": {"type": "ephemeral"},
            }],
            "messages": [{"role": "user", "content": self.user_content(request)}],
        });
        let object = body.as_object_mut().expect("body is an object");
        if let Some(budget) = self.thinking_budget {
            object.insert(
                "thinking".to_string(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
        }
        if let Some(extra) = &self.extra_body {
            for (field, value) in extra {
                object.insert(field.clone(), value.clone());
            }
        }
        body
    }

    /// The user message carries the schema instruction because Anthropic has
    /// no native structured output mode.
    fn user_content(&self, request: &CompletionRequest) -> String {
        format!(
            "Respond with only a JSON object conforming to this schema, no prose:\n{}\n\n{}",
            request.schema, request.user
        )
    }
}

impl Provider for AnthropicClient {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let body = self.body(request);
        let url = format!("{}/v1/messages", self.base_url);
        let mut call = self
            .http
            .post(url)
            .header("x-api-key", &self.key)
            .header("anthropic-version", ANTHROPIC_VERSION)
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
        let retry_after = super::openai::parse_retry_after(response.headers());
        let text = response
            .text()
            .await
            .map_err(|err| ProviderError::Request {
                message: redact(&err.to_string(), &self.key),
            })?;
        if !status.is_success() {
            return Err(super::with_request_excerpt(
                anthropic_error(status.as_u16(), &text, retry_after, &self.key),
                request,
            ));
        }
        let parsed: WireResponse =
            serde_json::from_str(&text).map_err(|err| ProviderError::Malformed {
                message: redact(
                    &format!("body is not a messages response: {err}"),
                    &self.key,
                ),
                usage: TokenUsage::default(),
            })?;
        let joined = parsed
            .content
            .iter()
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.as_str()),
                Block::Other => None,
            })
            .collect::<Vec<_>>()
            .join("");
        let usage = wire_usage(parsed.usage);
        if joined.trim().is_empty() {
            let detail = empty_content_detail(&text);
            if parsed.stop_reason.as_deref() == Some("max_tokens") {
                return Err(truncated_error(
                    &format!("{detail}; no text was produced"),
                    usage,
                    &text,
                    &self.key,
                ));
            }
            return Err(ProviderError::Malformed {
                message: redact(&detail, &self.key),
                usage,
            });
        }
        let content = match parse_json_content(&joined, usage) {
            Ok(value) => value,
            Err(err) => {
                if parsed.stop_reason.as_deref() == Some("max_tokens") {
                    return Err(truncated_error(
                        &format!("{err}; the ceiling cut the JSON mid-output"),
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

fn anthropic_error(
    status: u16,
    body: &str,
    retry_after: Option<std::time::Duration>,
    key: &str,
) -> ProviderError {
    let overflow = body.contains("prompt is too long") || body.contains("input length exceeds");
    let overloaded = body.contains("overloaded_error") || body.contains("overloaded");
    if status == 400 && overflow {
        ProviderError::ContextOverflow {
            message: body_excerpt(body),
        }
    } else if overloaded {
        ProviderError::Request {
            message: body_excerpt(body),
        }
    } else {
        classify(status, body, retry_after, key)
    }
}

#[derive(Debug, Deserialize)]
struct WireResponse {
    content: Vec<Block>,
    #[serde(default)]
    stop_reason: Option<String>,
    usage: Option<WireUsage>,
}

/// Describe a response that carried no text: the stop reason and the
/// content block types seen, which is what a gateway shape mismatch needs
/// to be diagnosed.
fn empty_content_detail(raw_body: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(raw_body).unwrap_or(serde_json::Value::Null);
    let stop_reason = parsed["stop_reason"].as_str().unwrap_or("unknown");
    let block_types: Vec<String> = parsed["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .map(|block| block["type"].as_str().unwrap_or("?").to_string())
                .collect()
        })
        .unwrap_or_default();
    format!(
        "empty response content: stop_reason={stop_reason}, block_types={block_types:?}; \
raw response excerpt: {}",
        body_excerpt(raw_body)
    )
}

/// Build an OutputTruncated error carrying the wasted call's usage so
/// spend stays honest.
/// Convert the wire usage split, counting cache creation as full-price
/// input and cache reads as the cached price.
fn wire_usage(usage: Option<WireUsage>) -> TokenUsage {
    let usage = usage.unwrap_or_default();
    TokenUsage {
        input_tokens: usage
            .input_tokens
            .saturating_add(usage.cache_creation_input_tokens.unwrap_or(0)),
        cached_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
        output_tokens: usage.output_tokens,
    }
}

fn truncated_error(detail: &str, usage: TokenUsage, raw_body: &str, key: &str) -> ProviderError {
    ProviderError::OutputTruncated {
        message: redact(
            &format!("{detail}; raw response excerpt: {}", body_excerpt(raw_body)),
            key,
        ),
        usage,
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum Block {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize, Default)]
struct WireUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Family, ModelDef, ProviderDef};
    use crate::provider::{RetryPolicy, complete_with_retries};
    use std::time::Duration;
    use wiremock::matchers::{body_partial_json, body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer, thinking_budget: Option<u32>) -> AnthropicClient {
        let provider = ProviderDef {
            family: Family::Anthropic,
            base_url: server.uri(),
            key_env: "TEST_KEY_ENV".to_string(),
            key_file: None,
            extra_body: None,
            extra_headers: Some(
                [("X-Custom", "custom-value")]
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
        };
        let model = ModelDef {
            provider: "test".to_string(),
            name: "claude-test".to_string(),
            input_price: 3.0,
            output_price: 15.0,
            reasoning_effort: None,
            thinking_budget,
            extra_body: None,
            extra_headers: None,
            cached_input_price: None,
        };
        AnthropicClient::new(&provider, &model, "sk-ant-test".to_string()).unwrap()
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

    fn success_body(text: String) -> Value {
        serde_json::json!({
            "content": [{"type": "text", "text": text}],
            "usage": {
                "input_tokens": 200,
                "output_tokens": 40,
                "cache_read_input_tokens": 800,
                "cache_creation_input_tokens": 20
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

    #[tokio::test]
    async fn request_carries_native_format_and_cache_marker() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .and(header("X-Custom", "custom-value"))
            .and(body_partial_json(serde_json::json!({
                "model": "claude-test",
                "max_tokens": 100,
                "system": [{"type": "text", "cache_control": {"type": "ephemeral"}}]
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(success_body("{\"verdict\": \"ok\"}".to_string())),
            )
            .expect(1)
            .mount(&server)
            .await;
        let response = complete_with_retries(&client(&server, None), &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(response.content, serde_json::json!({"verdict": "ok"}));
        server.verify().await;
    }

    #[tokio::test]
    async fn thinking_budget_translates_into_thinking_block() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(serde_json::json!({
                "thinking": {"type": "enabled", "budget_tokens": 8000},
                "max_tokens": 8100
            })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(success_body("{\"a\": 1}".to_string())),
            )
            .expect(1)
            .mount(&server)
            .await;
        complete_with_retries(&client(&server, Some(8000)), &request(), &fast_policy())
            .await
            .unwrap();
        server.verify().await;
    }

    #[tokio::test]
    async fn schema_instruction_enters_the_user_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains(
                "Respond with only a JSON object conforming to this schema",
            ))
            .and(body_string_contains("volatile data"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(success_body("{\"a\": 1}".to_string())),
            )
            .expect(1)
            .mount(&server)
            .await;
        complete_with_retries(&client(&server, None), &request(), &fast_policy())
            .await
            .unwrap();
        server.verify().await;
    }

    #[tokio::test]
    async fn cache_reads_are_reported_separately() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(success_body("{\"a\": 1}".to_string())),
            )
            .mount(&server)
            .await;
        let response = complete_with_retries(&client(&server, None), &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(response.usage.input_tokens, 220);
        assert_eq!(response.usage.cached_input_tokens, 800);
        assert_eq!(response.usage.output_tokens, 40);
    }

    #[tokio::test]
    async fn non_text_blocks_are_skipped() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "content": [
                {"type": "thinking", "thinking": "let me think"},
                {"type": "text", "text": "{\"a\": 1}"}
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let response = complete_with_retries(&client(&server, None), &request(), &fast_policy())
            .await
            .unwrap();
        assert_eq!(response.content, serde_json::json!({"a": 1}));
    }

    #[tokio::test]
    async fn prompt_too_long_is_context_overflow() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string("prompt is too long: 200000 tokens > 100000 maximum"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = complete_with_retries(&client(&server, None), &request(), &fast_policy())
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
                ResponseTemplate::new(401).set_body_string("invalid x-api-key sk-ant-test"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = complete_with_retries(&client(&server, None), &request(), &fast_policy())
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("sk-ant-test"));
        server.verify().await;
    }

    #[tokio::test]
    async fn overloaded_is_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(529).set_body_string("overloaded_error"))
            .expect(4)
            .mount(&server)
            .await;
        let err = complete_with_retries(&client(&server, None), &request(), &fast_policy())
            .await
            .unwrap_err();
        assert!(matches!(err, ProviderError::Request { .. }));
        server.verify().await;
    }

    #[tokio::test]
    async fn empty_content_error_reports_stop_reason_and_block_types() {
        let server = wiremock::MockServer::start().await;
        let body = serde_json::json!({
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "redacted_thinking", "data": "x"}
            ],
            "stop_reason": "max_tokens"
        });
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let client = client(&server, None);
        let err = client.complete(&request()).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("stop_reason=max_tokens"), "{text}");
        assert!(text.contains("thinking"), "{text}");
        assert!(text.contains("token ceiling"), "{text}");
    }

    #[tokio::test]
    async fn thinking_only_text_still_parses_when_present() {
        let server = wiremock::MockServer::start().await;
        let body = serde_json::json!({
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "{\"a\": 1}"}
            ],
            "stop_reason": "end_turn"
        });
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let client = client(&server, None);
        let response = client.complete(&request()).await.unwrap();
        assert_eq!(response.content, serde_json::json!({"a": 1}));
    }
}
