//! Response types for LLM provider clients.

use serde::{Deserialize, Serialize};

/// A chat-completion response from an LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    /// Text content returned by the model, if any.
    pub content: Option<String>,
    /// Tool calls requested by the model.
    pub tool_calls: Vec<ToolCall>,
    /// Model that produced the response.
    pub model: String,
    /// Provider-specific finish reason.
    pub finish_reason: String,
    /// Token usage reported by the provider.
    pub usage: TokenUsage,
}

/// Token usage statistics for a single LLM request/response.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Number of tokens in the prompt.
    pub prompt: u32,
    /// Number of tokens in the completion.
    pub completion: u32,
    /// Total number of tokens used.
    pub total: u32,
}

impl TokenUsage {
    /// Create a new usage record with `total` set to `prompt + completion`.
    pub fn new(prompt: u32, completion: u32) -> Self {
        Self {
            prompt,
            completion,
            total: prompt.saturating_add(completion),
        }
    }
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Classified error kind for LLM provider failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LlmErrorKind {
    Timeout,
    RateLimit,
    AuthError,
    ProviderError,
    FormatError,
    BudgetExceeded,
    Unknown,
}

impl std::str::FromStr for LlmErrorKind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Timeout" => Ok(LlmErrorKind::Timeout),
            "RateLimit" => Ok(LlmErrorKind::RateLimit),
            "AuthError" => Ok(LlmErrorKind::AuthError),
            "ProviderError" => Ok(LlmErrorKind::ProviderError),
            "FormatError" => Ok(LlmErrorKind::FormatError),
            "BudgetExceeded" => Ok(LlmErrorKind::BudgetExceeded),
            "Unknown" => Ok(LlmErrorKind::Unknown),
            _ => Err(()),
        }
    }
}

/// Structured error returned by LLM provider clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmError {
    pub kind: LlmErrorKind,
    pub message: String,
    /// Retry-After hint in milliseconds, if the provider supplied one.
    pub retry_after_ms: Option<u64>,
}

impl LlmError {
    /// Create a new error with the given kind and message.
    pub fn new(kind: LlmErrorKind, message: impl Into<String>) -> Self {
        LlmError {
            kind,
            message: message.into(),
            retry_after_ms: None,
        }
    }

    /// Create an `Unknown` error from a plain string (backward compat path).
    pub fn from_string(message: impl Into<String>) -> Self {
        LlmError {
            kind: LlmErrorKind::Unknown,
            message: message.into(),
            retry_after_ms: None,
        }
    }
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for LlmError {}

impl From<String> for LlmError {
    fn from(s: String) -> Self {
        LlmError::from_string(s)
    }
}

impl From<&str> for LlmError {
    fn from(s: &str) -> Self {
        LlmError::from_string(s)
    }
}
/// A single tool invocation produced by an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-specific tool-call identifier.
    pub id: String,
    /// Name of the tool to invoke.
    pub name: String,
    /// Parsed tool arguments.
    pub arguments: serde_json::Map<String, serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_response_construction() {
        let response = LlmResponse {
            content: Some("Hello".to_string()),
            tool_calls: vec![],
            model: "gpt-4".to_string(),
            finish_reason: "stop".to_string(),
            usage: TokenUsage::new(10, 5),
        };
        assert_eq!(response.content.as_deref(), Some("Hello"));
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.model, "gpt-4");
        assert_eq!(response.finish_reason, "stop");
        assert_eq!(response.usage.prompt, 10);
        assert_eq!(response.usage.completion, 5);
        assert_eq!(response.usage.total, 15);
    }

    #[test]
    fn test_token_usage_new() {
        let usage = TokenUsage::new(100, 50);
        assert_eq!(usage.prompt, 100);
        assert_eq!(usage.completion, 50);
        assert_eq!(usage.total, 150);
    }

    #[test]
    fn test_token_usage_default() {
        let usage = TokenUsage::default();
        assert_eq!(usage.prompt, 0);
        assert_eq!(usage.completion, 0);
        assert_eq!(usage.total, 0);
    }

    #[test]
    fn test_token_usage_total_saturates_instead_of_overflowing() {
        let overflow = TokenUsage::new(u32::MAX, 1);
        assert_eq!(overflow.prompt, u32::MAX);
        assert_eq!(overflow.completion, 1);
        assert_eq!(overflow.total, u32::MAX);

        let both_max = TokenUsage::new(u32::MAX, u32::MAX);
        assert_eq!(both_max.total, u32::MAX);
    }

    #[test]
    fn test_tool_call_construction() {
        let mut args = serde_json::Map::new();
        args.insert(
            "location".to_string(),
            serde_json::Value::String("NYC".to_string()),
        );
        let call = ToolCall {
            id: "call_1".to_string(),
            name: "get_weather".to_string(),
            arguments: args,
        };
        assert_eq!(call.id, "call_1");
        assert_eq!(call.name, "get_weather");
        assert_eq!(call.arguments.get("location").unwrap(), "NYC");
    }
}
