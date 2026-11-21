//! OpenAI-compatible LLM provider client.
//!
//! Supports the standard `https://api.openai.com/v1/chat/completions` endpoint
//! as well as any OpenAI-compatible API by configuring a custom `base_url`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::client::LlmClient;
use crate::request::{LlmMessage, LlmRequest, ToolSchema};
use crate::response::{LlmError, LlmErrorKind, LlmResponse, TokenUsage, ToolCall};

/// Client for the OpenAI chat completions API.
#[derive(Debug, Clone)]
pub struct OpenAiClient {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

#[async_trait]
impl LlmClient for OpenAiClient {
    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let model = if request.model.is_empty() {
            self.model.clone()
        } else {
            request.model
        };

        let messages: Vec<LlmMessage> =
            request.memory.into_iter().chain(request.messages).collect();

        let tools: Vec<OpenAiTool> = request.tools.into_iter().map(into_openai_tool).collect();
        let tool_choice = if tools.is_empty() {
            None
        } else {
            Some(serde_json::json!("auto"))
        };

        let body = OpenAiChatRequest {
            model,
            messages,
            tools: if tools.is_empty() { None } else { Some(tools) },
            tool_choice,
            stream: false,
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(classify_reqwest_error)?;

        let status = response.status();
        if !status.is_success() {
            let status_text = response.text().await.unwrap_or_default();
            return Err(classify_http_error(status.as_u16(), &status_text));
        }

        let response: OpenAiChatResponse = response.json().await.map_err(|e| {
            LlmError::new(
                LlmErrorKind::FormatError,
                format!("OpenAI response parse failed: {}", e),
            )
        })?;

        let choice = response.choices.into_iter().next().ok_or_else(|| {
            LlmError::new(
                LlmErrorKind::FormatError,
                "OpenAI response contained no choices",
            )
        })?;

        let message = choice.message;
        let content = if message
            .content
            .as_ref()
            .map(|s| s.is_empty())
            .unwrap_or(true)
        {
            None
        } else {
            message.content
        };

        let tool_calls = parse_tool_calls(message.tool_calls.unwrap_or_default())?;

        let usage = response.usage.unwrap_or_default();

        Ok(LlmResponse {
            content,
            tool_calls,
            model: response.model,
            finish_reason: choice.finish_reason.unwrap_or_default(),
            usage: TokenUsage::new(usage.prompt_tokens, usage.completion_tokens),
        })
    }
}

/// Parse provider tool calls without inventing empty arguments for invalid data.
/// Fail the entire response rather than dispatching a partially parsed batch.
fn parse_tool_calls(calls: Vec<OpenAiToolCall>) -> Result<Vec<ToolCall>, LlmError> {
    calls
        .into_iter()
        .map(|call| {
            let arguments = match call.function.arguments {
                serde_json::Value::String(encoded) => {
                    serde_json::from_str::<serde_json::Value>(&encoded).map_err(|_| {
                        LlmError::new(
                            LlmErrorKind::FormatError,
                            "OpenAI tool call arguments contain invalid JSON",
                        )
                    })?
                }
                value => value,
            };
            let serde_json::Value::Object(arguments) = arguments else {
                return Err(LlmError::new(
                    LlmErrorKind::FormatError,
                    "OpenAI tool call arguments must be a JSON object",
                ));
            };
            Ok(ToolCall {
                id: call.id,
                name: call.function.name,
                arguments,
            })
        })
        .collect()
}

// -- Error classification helpers -------------------------------------------

/// Classify a `reqwest::Error` into an `LlmError`. Timeout -> Timeout,
/// everything else -> ProviderError with the error text.
fn classify_reqwest_error(e: reqwest::Error) -> LlmError {
    if e.is_timeout() {
        LlmError::new(
            LlmErrorKind::Timeout,
            format!("OpenAI request timed out: {}", e),
        )
    } else if e.is_connect() {
        LlmError::new(
            LlmErrorKind::ProviderError,
            format!("OpenAI connection failed: {}", e),
        )
    } else {
        LlmError::new(
            LlmErrorKind::ProviderError,
            format!("OpenAI request failed: {}", e),
        )
    }
}

/// Classify an HTTP error status into an `LlmError`.
fn classify_http_error(status: u16, body: &str) -> LlmError {
    let msg = format!("OpenAI HTTP {}: {}", status, body);
    match status {
        408 => LlmError::new(LlmErrorKind::Timeout, msg),
        429 => LlmError::new(LlmErrorKind::RateLimit, msg),
        401 | 403 => LlmError::new(LlmErrorKind::AuthError, msg),
        s if s >= 500 => LlmError::new(LlmErrorKind::ProviderError, msg),
        _ => LlmError::new(LlmErrorKind::Unknown, msg),
    }
}

impl OpenAiClient {
    /// Create a new OpenAI client with the given API key and default model.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: api_key.into(),
            model: model.into(),
            client: crate::client::http_client(),
        }
    }

    /// Create a client pointing at a custom OpenAI-compatible endpoint.
    pub fn with_base_url(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            client: crate::client::http_client(),
        }
    }

    /// Default client for GPT-4o. Reads the `OPENAI_API_KEY` environment
    /// variable; returns an error-string client if the variable is missing.
    pub fn gpt4o() -> Result<Self, String> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| "OPENAI_API_KEY environment variable not set".to_string())?;
        Ok(Self::new(api_key, "gpt-4o"))
    }
}

fn into_openai_tool(tool: ToolSchema) -> OpenAiTool {
    OpenAiTool {
        ty: "function".to_string(),
        function: OpenAiFunction {
            name: tool.name,
            description: tool.description,
            parameters: tool.parameters,
        },
    }
}

#[derive(Debug, Serialize)]
struct OpenAiChatRequest {
    model: String,
    messages: Vec<LlmMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiTool>>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tool_choice")]
    tool_choice: Option<serde_json::Value>,
    stream: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAiTool {
    #[serde(rename = "type")]
    ty: String,
    function: OpenAiFunction,
}

#[derive(Debug, Serialize, Deserialize)]
struct OpenAiFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatResponse {
    model: String,
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiMessage,
    #[serde(rename = "finish_reason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiMessage {
    content: Option<String>,
    #[serde(rename = "tool_calls")]
    tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Debug, Deserialize, Clone)]
struct OpenAiToolCall {
    id: String,
    function: OpenAiToolCallFunction,
}

#[derive(Debug, Deserialize, Clone)]
struct OpenAiToolCallFunction {
    name: String,
    /// OpenAI returns arguments as a JSON string.
    arguments: serde_json::Value,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_openai_client_default_model() {
        let client = OpenAiClient::new("test-key", "gpt-4o-mini");
        assert_eq!(client.model, "gpt-4o-mini");
        assert_eq!(client.base_url, "https://api.openai.com/v1");
    }

    #[test]
    fn test_openai_client_custom_base_url() {
        let client = OpenAiClient::with_base_url("https://api.example.com/v1", "key", "model");
        assert_eq!(client.base_url, "https://api.example.com/v1");
    }

    #[test]
    fn test_into_openai_tool_preserves_schema() {
        let schema = ToolSchema {
            name: "get_weather".to_string(),
            description: "Get the weather".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "location": { "type": "string" }
                }
            }),
        };
        let tool = into_openai_tool(schema);
        assert_eq!(tool.function.name, "get_weather");
        assert_eq!(tool.ty, "function");
    }

    #[test]
    fn test_parse_openai_response() {
        let json = json!({
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": "Hello!",
                    "role": "assistant"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5
            }
        });
        let response: OpenAiChatResponse = serde_json::from_value(json).unwrap();
        assert_eq!(response.model, "gpt-4o");
        assert_eq!(response.choices.len(), 1);
        assert_eq!(
            response.choices[0].message.content.as_deref(),
            Some("Hello!")
        );
        assert_eq!(response.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = response.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
    }

    #[test]
    fn test_parse_openai_tool_call_response() {
        let json = json!({
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"location\":\"NYC\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 10
            }
        });
        let response: OpenAiChatResponse = serde_json::from_value(json).unwrap();
        let tool_call = response.choices[0].message.tool_calls.as_ref().unwrap()[0].clone();
        assert_eq!(tool_call.id, "call_1");
        assert_eq!(tool_call.function.name, "get_weather");
        assert_eq!(
            tool_call.function.arguments,
            json!("{\"location\":\"NYC\"}")
        );
    }

    #[test]
    fn test_argument_string_parsing_roundtrip() {
        let call = OpenAiToolCall {
            id: "call_1".into(),
            function: OpenAiToolCallFunction {
                name: "get_weather".into(),
                arguments: json!("{\"location\":\"NYC\"}"),
            },
        };
        let parsed = parse_tool_calls(vec![call]).unwrap();
        assert_eq!(parsed[0].arguments.get("location"), Some(&json!("NYC")));
    }

    #[test]
    fn tool_calls_accept_object_and_json_encoded_object_arguments() {
        let calls = vec![
            OpenAiToolCall {
                id: "direct".into(),
                function: OpenAiToolCallFunction {
                    name: "lookup".into(),
                    arguments: json!({"id": 42}),
                },
            },
            OpenAiToolCall {
                id: "encoded".into(),
                function: OpenAiToolCallFunction {
                    name: "lookup".into(),
                    arguments: json!("{\"id\":43}"),
                },
            },
        ];
        let parsed = parse_tool_calls(calls).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].arguments.get("id"), Some(&json!(42)));
        assert_eq!(parsed[1].arguments.get("id"), Some(&json!(43)));
    }

    #[test]
    fn tool_calls_reject_malformed_json_instead_of_executing_with_empty_arguments() {
        let calls = vec![OpenAiToolCall {
            id: "bad".into(),
            function: OpenAiToolCallFunction {
                name: "delete_resource".into(),
                arguments: json!("{\"resource\":"),
            },
        }];
        let error = parse_tool_calls(calls).unwrap_err();
        assert_eq!(error.kind, LlmErrorKind::FormatError);
    }

    #[test]
    fn tool_calls_reject_non_object_json_arguments() {
        for arguments in [
            json!("[]"),
            json!("null"),
            json!([1, 2]),
            json!(null),
            json!(42),
        ] {
            let calls = vec![OpenAiToolCall {
                id: "bad".into(),
                function: OpenAiToolCallFunction {
                    name: "delete_resource".into(),
                    arguments,
                },
            }];
            let error = parse_tool_calls(calls).unwrap_err();
            assert_eq!(error.kind, LlmErrorKind::FormatError);
        }
    }

    #[test]
    fn tool_calls_reject_entire_batch_when_later_call_is_malformed() {
        let calls = vec![
            OpenAiToolCall {
                id: "valid".into(),
                function: OpenAiToolCallFunction {
                    name: "lookup".into(),
                    arguments: json!({"id": 1}),
                },
            },
            OpenAiToolCall {
                id: "invalid".into(),
                function: OpenAiToolCallFunction {
                    name: "delete_resource".into(),
                    arguments: json!("not-json"),
                },
            },
        ];
        assert_eq!(
            parse_tool_calls(calls).unwrap_err().kind,
            LlmErrorKind::FormatError
        );
    }
}
