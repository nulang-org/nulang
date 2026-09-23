---
title: Provider Contract Evals
description: Run deterministic provider and gateway contract checks before wiring an LLM client into Nulang agents.
---

## Provider Contract Evals

Nulang's `LlmClient` boundary is intentionally provider-agnostic. Before a
provider or gateway is used by an agent, deterministic contract evals can check
the normalized behavior that the runtime depends on without using another model
as a judge.

The eval helpers validate runtime-critical invariants:

- the normalized response has a non-empty model identifier;
- provider token accounting is internally consistent;
- the response contains text or at least one tool call;
- the provider cannot return a tool call for a tool the request never exposed;
- a test case can require text fragments, expected tool names, and a maximum
  provider-reported token count.

These checks measure adapter and gateway conformance. They do **not** establish
factual correctness, model quality, prompt-injection resistance, or durable
external-effect semantics.

## Run a deterministic case

```rust
use nulang_ai::{
    run_evals, EvalCase, LlmMessage, LlmRequest, OpenAiClient, TokenUsage,
};

# async fn example() {
let client = OpenAiClient::with_base_url(
    "https://gateway.example.com/v1",
    std::env::var("GATEWAY_API_KEY").expect("GATEWAY_API_KEY"),
    "my-model",
);

let request = LlmRequest {
    model: "my-model".into(),
    messages: vec![LlmMessage {
        role: "user".into(),
        content: "Reply with the word READY.".into(),
    }],
    ..LlmRequest::default()
};

let mut case = EvalCase::new("basic-response", request);
case.expectation.content_contains.push("READY".into());
case.expectation.max_total_tokens = Some(100);

let results = run_evals(&client, &[case]).await;
assert!(results[0].passed, "{:?}", results[0].failures);
# }
```

`OpenAiClient::with_base_url` works with APIs that implement the OpenAI chat
completions wire format. Native provider adapters can implement `LlmClient`
directly and run the same eval cases.

## Tool exposure contract

A provider response that asks Nulang to invoke an unexposed tool is a contract
violation:

```rust
use nulang_ai::provider_contract_violations;

let failures = provider_contract_violations(&request, &response);
assert!(failures.is_empty(), "{failures:?}");
```

This is defense in depth, not authorization. Tool execution still needs the
runtime's authority checks and application-specific approval policy.

## Production gate

A provider/gateway should be promoted independently for each configuration that
can materially change behavior: endpoint, model family, tool-calling mode, and
structured-output mode.

Keep provider availability failures separate from contract failures. The eval
result preserves the classified `LlmError` so a rate limit, authentication
failure, timeout, or provider outage is not misreported as a model-quality
regression.

For external calls that must survive process failure, provider conformance is
only one layer. Durable execution additionally requires a stable operation
identity, an intent persisted before dispatch, request fingerprint validation,
and receipt-backed replay. See the repository's durable execution guarantees
before making effectively-once claims.
