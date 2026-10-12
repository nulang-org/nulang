//! Deterministic provider-contract and AI evaluation helpers.
//!
//! These checks are deliberately model-judge-free. They validate the wire
//! contract Nulang depends on before subjective quality evaluation is layered
//! on top.

use crate::{LlmClient, LlmError, LlmRequest, LlmResponse};

#[derive(Debug, Clone)]
pub struct EvalExpectation {
    /// Substrings that must all appear in textual model output.
    pub content_contains: Vec<String>,
    /// Tool names that must appear at least once in the response.
    pub expected_tool_names: Vec<String>,
    /// Optional hard ceiling for provider-reported total tokens.
    pub max_total_tokens: Option<u32>,
    /// Require either non-empty text or at least one tool call.
    pub require_nonempty_output: bool,
}

impl Default for EvalExpectation {
    fn default() -> Self {
        Self {
            content_contains: Vec::new(),
            expected_tool_names: Vec::new(),
            max_total_tokens: None,
            require_nonempty_output: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EvalCase {
    pub name: String,
    pub request: LlmRequest,
    pub expectation: EvalExpectation,
}

impl EvalCase {
    pub fn new(name: impl Into<String>, request: LlmRequest) -> Self {
        Self {
            name: name.into(),
            request,
            expectation: EvalExpectation::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EvalResult {
    pub name: String,
    pub passed: bool,
    pub failures: Vec<String>,
    pub response: Option<LlmResponse>,
    pub provider_error: Option<LlmError>,
}

/// Validate invariants that every provider adapter must preserve.
///
/// This catches adapter/gateway normalization failures independently of model
/// quality: empty model identifiers, impossible usage totals, empty responses,
/// and tool calls for tools the request never exposed.
pub fn provider_contract_violations(
    request: &LlmRequest,
    response: &LlmResponse,
) -> Vec<String> {
    let mut failures = Vec::new();

    if response.model.trim().is_empty() {
        failures.push("response model identifier is empty".to_string());
    }

    if response.usage.prompt.checked_add(response.usage.completion)
        != Some(response.usage.total)
    {
        failures.push(format!(
            "token usage mismatch: prompt {} + completion {} != total {}",
            response.usage.prompt, response.usage.completion, response.usage.total
        ));
    }

    let has_text = response
        .content
        .as_deref()
        .is_some_and(|content| !content.trim().is_empty());
    if !has_text && response.tool_calls.is_empty() {
        failures.push("response contains neither text nor tool calls".to_string());
    }

    let exposed_tools: std::collections::HashSet<&str> =
        request.tools.iter().map(|tool| tool.name.as_str()).collect();

    for call in &response.tool_calls {
        if call.name.trim().is_empty() {
            failures.push("tool call has an empty name".to_string());
            continue;
        }
        if !exposed_tools.contains(call.name.as_str()) {
            failures.push(format!(
                "provider returned unexposed tool call: {}",
                call.name
            ));
        }
    }

    failures
}

/// Run deterministic evaluation cases through one provider/gateway client.
///
/// Provider errors fail the case but remain separately classified so callers
/// can distinguish quality regressions from availability/auth/rate-limit
/// failures.
pub async fn run_evals(client: &dyn LlmClient, cases: &[EvalCase]) -> Vec<EvalResult> {
    let mut results = Vec::with_capacity(cases.len());

    for case in cases {
        match client.complete(case.request.clone()).await {
            Ok(response) => {
                let mut failures = provider_contract_violations(&case.request, &response);

                if case.expectation.require_nonempty_output {
                    let has_text = response
                        .content
                        .as_deref()
                        .is_some_and(|content| !content.trim().is_empty());
                    if !has_text && response.tool_calls.is_empty() {
                        // Avoid duplicate wording if the provider contract
                        // already caught this exact condition.
                        if !failures
                            .iter()
                            .any(|failure| failure == "response contains neither text nor tool calls")
                        {
                            failures.push("response contains neither text nor tool calls".into());
                        }
                    }
                }

                let content = response.content.as_deref().unwrap_or_default();
                for expected in &case.expectation.content_contains {
                    if !content.contains(expected) {
                        failures.push(format!(
                            "text output does not contain required substring: {expected:?}"
                        ));
                    }
                }

                for expected_tool in &case.expectation.expected_tool_names {
                    if !response
                        .tool_calls
                        .iter()
                        .any(|call| call.name == *expected_tool)
                    {
                        failures.push(format!(
                            "missing expected tool call: {expected_tool}"
                        ));
                    }
                }

                if let Some(max_tokens) = case.expectation.max_total_tokens {
                    if response.usage.total > max_tokens {
                        failures.push(format!(
                            "token budget exceeded: {} > {}",
                            response.usage.total, max_tokens
                        ));
                    }
                }

                results.push(EvalResult {
                    name: case.name.clone(),
                    passed: failures.is_empty(),
                    failures,
                    response: Some(response),
                    provider_error: None,
                });
            }
            Err(error) => {
                results.push(EvalResult {
                    name: case.name.clone(),
                    passed: false,
                    failures: vec![format!("provider error: {error}")],
                    response: None,
                    provider_error: Some(error),
                });
            }
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LlmMessage, MockLlmClient, TokenUsage, ToolCall, ToolSchema};
    use serde_json::json;

    fn request_with_tool() -> LlmRequest {
        LlmRequest {
            model: "mock".into(),
            messages: vec![LlmMessage {
                role: "user".into(),
                content: "weather".into(),
            }],
            tools: vec![ToolSchema {
                name: "get_weather".into(),
                description: "weather".into(),
                parameters: json!({"type": "object"}),
            }],
            ..LlmRequest::default()
        }
    }

    #[test]
    fn provider_contract_rejects_unexposed_tool_calls() {
        let request = request_with_tool();
        let response = LlmResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "delete_database".into(),
                arguments: serde_json::Map::new(),
            }],
            model: "mock".into(),
            finish_reason: "tool_calls".into(),
            usage: TokenUsage::new(10, 5),
        };

        let failures = provider_contract_violations(&request, &response);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("unexposed tool call"));
    }

    #[test]
    fn provider_contract_rejects_usage_mismatch() {
        let response = LlmResponse {
            content: Some("ok".into()),
            tool_calls: vec![],
            model: "mock".into(),
            finish_reason: "stop".into(),
            usage: TokenUsage {
                prompt: 10,
                completion: 5,
                total: 99,
            },
        };

        let failures = provider_contract_violations(&LlmRequest::default(), &response);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("token usage mismatch"));
    }

    #[test]
    fn deterministic_eval_passes_text_expectation() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let client = MockLlmClient::with_usage("hello world", TokenUsage::new(4, 2));
        let mut case = EvalCase::new("hello", LlmRequest::default());
        case.expectation.content_contains.push("world".into());
        case.expectation.max_total_tokens = Some(10);

        let results = runtime.block_on(run_evals(&client, &[case]));
        assert_eq!(results.len(), 1);
        assert!(results[0].passed, "{:?}", results[0].failures);
    }

    #[test]
    fn deterministic_eval_reports_missing_expected_tool() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let client = MockLlmClient::text("no tool");
        let mut case = EvalCase::new("tool-use", request_with_tool());
        case.expectation
            .expected_tool_names
            .push("get_weather".into());

        let results = runtime.block_on(run_evals(&client, &[case]));
        assert!(!results[0].passed);
        assert!(results[0]
            .failures
            .iter()
            .any(|failure| failure.contains("missing expected tool call")));
    }
}
