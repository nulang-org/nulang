from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected one replacement, found {count}: {old[:100]!r}")
    p.write_text(text.replace(old, new, 1))


# ---------------------------------------------------------------------------
# Provider contract: distinguish local/in-memory clients from real networks.
# Unknown custom clients deliberately default to Unspecified so actor-backed
# execution cannot silently gain ambient network authority.
# ---------------------------------------------------------------------------
replace_once(
    "crates/nulang-ai/src/client.rs",
    '''/// Async trait implemented by all LLM provider clients.\n#[async_trait]\npub trait LlmClient: Send + Sync {\n    /// Request a chat completion from the provider.\n    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError>;\n}\n''',
    '''/// External-resource authority required by one LLM client implementation.\n///\n/// `Local` clients never cross a network boundary (for example deterministic\n/// test/memory clients). `Network` clients name the concrete endpoint whose\n/// TCP destination must be authorized by an actor runtime. `Unspecified` is\n/// the fail-closed default for third-party clients until they declare which\n/// side of the boundary they live on.\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum LlmClientAuthority<'a> {\n    Local,\n    Network { endpoint: &'a str },\n    Unspecified,\n}\n\n/// Async trait implemented by all LLM provider clients.\n#[async_trait]\npub trait LlmClient: Send + Sync {\n    /// Describe the external-resource boundary crossed by this client.\n    ///\n    /// The default is intentionally fail-closed for actor-backed execution.\n    /// Existing callers that execute outside an actor remain ambient.\n    fn authority(&self) -> LlmClientAuthority<'_> {\n        LlmClientAuthority::Unspecified\n    }\n\n    /// Request a chat completion from the provider.\n    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError>;\n}\n''',
)

replace_once(
    "crates/nulang-ai/src/client.rs",
    '''    reqwest::Client::builder()\n        .timeout(std::time::Duration::from_secs(LLM_HTTP_TIMEOUT_SECS))\n        .build()\n        .unwrap_or_else(|_| reqwest::Client::new())\n''',
    '''    reqwest::Client::builder()\n        // Exact Net::TcpOut authority applies to one destination. Following an\n        // automatic redirect could otherwise turn an authorized endpoint into\n        // an un-authorized second connection. Provider code may surface 3xx\n        // responses explicitly, but the HTTP layer must never widen authority.\n        .redirect(reqwest::redirect::Policy::none())\n        .timeout(std::time::Duration::from_secs(LLM_HTTP_TIMEOUT_SECS))\n        .build()\n        .expect("static LLM HTTP client configuration must be valid")\n''',
)

replace_once(
    "crates/nulang-ai/src/client.rs",
    '''    #[test]\n    fn test_http_client_builds() {\n        // The shared provider client builder must succeed (timeout config is\n        // not observable through the reqwest API, so construction is what we\n        // can assert here).\n        let _client = http_client();\n    }\n''',
    '''    #[test]\n    fn test_http_client_builds() {\n        // The shared provider client builder must succeed.\n        let _client = http_client();\n    }\n\n    #[test]\n    fn test_http_client_does_not_follow_redirects() {\n        use std::io::{Read, Write};\n        use std::net::TcpListener;\n\n        let listener = TcpListener::bind("127.0.0.1:0").unwrap();\n        let addr = listener.local_addr().unwrap();\n        let server = std::thread::spawn(move || {\n            let (mut stream, _) = listener.accept().unwrap();\n            let mut request = [0u8; 1024];\n            let _ = stream.read(&mut request);\n            stream\n                .write_all(\n                    b"HTTP/1.1 302 Found\\r\\nLocation: http://127.0.0.1:1/blocked\\r\\nContent-Length: 0\\r\\nConnection: close\\r\\n\\r\\n",\n                )\n                .unwrap();\n        });\n\n        let rt = tokio::runtime::Builder::new_current_thread()\n            .enable_all()\n            .build()\n            .unwrap();\n        let response = rt\n            .block_on(http_client().get(format!("http://{addr}/start")).send())\n            .expect("redirect response should be returned without dialing its Location");\n        assert_eq!(response.status().as_u16(), 302);\n        server.join().unwrap();\n    }\n''',
)

# Mock clients are explicitly local. Real providers expose their configured
# base endpoint; no network call is required to inspect this metadata.
replace_once(
    "crates/nulang-ai/src/mock.rs",
    '''use crate::client::LlmClient;\n''',
    '''use crate::client::{LlmClient, LlmClientAuthority};\n''',
)
replace_once(
    "crates/nulang-ai/src/mock.rs",
    '''#[async_trait]\nimpl LlmClient for MockLlmClient {\n    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {\n''',
    '''#[async_trait]\nimpl LlmClient for MockLlmClient {\n    fn authority(&self) -> LlmClientAuthority<'_> {\n        LlmClientAuthority::Local\n    }\n\n    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {\n''',
)

for provider in ("openai", "ollama"):
    path = f"crates/nulang-ai/src/providers/{provider}.rs"
    replace_once(
        path,
        '''use crate::client::LlmClient;\n''',
        '''use crate::client::{LlmClient, LlmClientAuthority};\n''',
    )

replace_once(
    "crates/nulang-ai/src/providers/openai.rs",
    '''#[async_trait]\nimpl LlmClient for OpenAiClient {\n    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {\n''',
    '''#[async_trait]\nimpl LlmClient for OpenAiClient {\n    fn authority(&self) -> LlmClientAuthority<'_> {\n        LlmClientAuthority::Network {\n            endpoint: &self.base_url,\n        }\n    }\n\n    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {\n''',
)
replace_once(
    "crates/nulang-ai/src/providers/ollama.rs",
    '''#[async_trait]\nimpl LlmClient for OllamaClient {\n    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {\n''',
    '''#[async_trait]\nimpl LlmClient for OllamaClient {\n    fn authority(&self) -> LlmClientAuthority<'_> {\n        LlmClientAuthority::Network {\n            endpoint: &self.base_url,\n        }\n    }\n\n    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError> {\n''',
)

replace_once(
    "crates/nulang-ai/src/mod.rs",
    '''pub use client::{complete_sync, LlmClient};\n''',
    '''pub use client::{complete_sync, LlmClient, LlmClientAuthority};\n''',
)

# ---------------------------------------------------------------------------
# Core runtime: authorize the concrete provider boundary, not the logical
# Inference/Provider effect. This helper is shared by synchronous agent calls
# and the async actor-worker dispatch path.
# ---------------------------------------------------------------------------
replace_once(
    "src/runtime/callbacks.rs",
    '''fn http_outbound_authority(url: &str) -> Result<crate::authority::AuthorityGrant, String> {\n''',
    '''pub(crate) fn http_outbound_authority(\n    url: &str,\n) -> Result<crate::authority::AuthorityGrant, String> {\n''',
)

replace_once(
    "src/runtime/agent.rs",
    '''use nulang_ai::{\n    EpisodicMemory, LlmClient, LlmMessage, LlmRequest, LlmResponse, ModelPricing, TokenBudget,\n};\n''',
    '''use nulang_ai::{\n    EpisodicMemory, LlmClient, LlmClientAuthority, LlmMessage, LlmRequest, LlmResponse,\n    ModelPricing, TokenBudget,\n};\n''',
)

replace_once(
    "src/runtime/agent.rs",
    '''pub(crate) fn set_llm_client(rt: &mut Runtime, client: Box<dyn LlmClient>) {\n    rt.llm.client = Some(Arc::from(client));\n}\n\n''',
    '''pub(crate) fn set_llm_client(rt: &mut Runtime, client: Box<dyn LlmClient>) {\n    rt.llm.client = Some(Arc::from(client));\n}\n\n/// Authorize the concrete external resource used by an LLM client.\n///\n/// Logical `Inference.ask` / `Provider.ask` operations are deliberately not\n/// capabilities themselves. Local clients stay authority-neutral; network\n/// clients require the exact TCP destination implied by their configured\n/// endpoint. Actor-free/top-level execution retains the established ambient\n/// contract. Unknown custom clients fail closed for actor-backed execution.\npub(crate) fn authorize_llm_client(\n    rt: &Runtime,\n    actor_id: Option<u64>,\n    client: &dyn LlmClient,\n) -> Result<(), nulang_ai::LlmError> {\n    let Some(actor_id) = actor_id.filter(|id| *id != 0) else {\n        return Ok(());\n    };\n    let actor = rt\n        .actors\n        .get(&actor_id)\n        .ok_or_else(|| nulang_ai::LlmError::from_string(format!(\n            "LLM authority source actor {actor_id} is missing"\n        )))?;\n\n    match client.authority() {\n        LlmClientAuthority::Local => Ok(()),\n        LlmClientAuthority::Network { endpoint } => {\n            let grant = crate::runtime::callbacks::http_outbound_authority(endpoint)\n                .map_err(|error| nulang_ai::LlmError::from_string(format!(\n                    "invalid LLM provider endpoint authority: {error}"\n                )))?;\n            actor\n                .require_authority(&grant)\n                .map_err(|error| nulang_ai::LlmError::from_string(format!(\n                    "LLM provider authority denied: {error}"\n                )))\n        }\n        LlmClientAuthority::Unspecified => Err(nulang_ai::LlmError::from_string(\n            "LLM client authority is unspecified for actor-backed execution",\n        )),\n    }\n}\n\n''',
)

replace_once(
    "src/runtime/agent.rs",
    '''    let client = rt\n        .llm\n        .client\n        .as_ref()\n        .ok_or_else(|| nulang_ai::LlmError::from_string("No LLM client configured"))?;\n    let response = nulang_ai::complete_sync(client.as_ref(), request)?;\n''',
    '''    let client = rt\n        .llm\n        .client\n        .as_ref()\n        .ok_or_else(|| nulang_ai::LlmError::from_string("No LLM client configured"))?;\n    authorize_llm_client(rt, rt.current_actor, client.as_ref())?;\n    let response = nulang_ai::complete_sync(client.as_ref(), request)?;\n''',
)

# Async dispatch checks the same helper before publishing work or marking the
# actor in-flight, so denial has no externally-observable side effect.
replace_once(
    "src/runtime/llm.rs",
    '''    let Some(client) = rt.llm.client.clone() else {\n        return false;\n    };\n    let Some(tx) = rt.llm_executor.tx.as_ref() else {\n''',
    '''    let Some(client) = rt.llm.client.clone() else {\n        return false;\n    };\n    if super::agent::authorize_llm_client(rt, Some(actor_id), client.as_ref()).is_err() {\n        return false;\n    }\n    let Some(tx) = rt.llm_executor.tx.as_ref() else {\n''',
)

# Unit regressions exercise policy without issuing external network requests.
agent = Path("src/runtime/agent.rs")
text = agent.read_text()
if "fn network_llm_client_requires_exact_endpoint_authority()" in text:
    raise SystemExit("agent authority tests already installed")
text += '''\n\n#[cfg(test)]\nmod llm_authority_tests {\n    use super::*;\n    use crate::authority::AuthorityManifest;\n    use nulang_ai::{MockLlmClient, OpenAiClient};\n\n    fn actor_runtime() -> (Runtime, u64) {\n        let mut rt = Runtime::new();\n        let actor_id = rt.spawn_actor(Box::new(|| vec![]));\n        (rt, actor_id)\n    }\n\n    #[test]\n    fn local_llm_client_is_authority_neutral() {\n        let (rt, actor_id) = actor_runtime();\n        let client = MockLlmClient::text("ok");\n        assert!(authorize_llm_client(&rt, Some(actor_id), &client).is_ok());\n    }\n\n    #[test]\n    fn network_llm_client_requires_exact_endpoint_authority() {\n        let (mut rt, actor_id) = actor_runtime();\n        let client = OpenAiClient::with_base_url(\n            "https://api.example.com/v1",\n            "test-key",\n            "test-model",\n        );\n\n        assert!(authorize_llm_client(&rt, Some(actor_id), &client).is_err());\n\n        let wrong = AuthorityManifest::from_tokens([\n            "Net::TcpOut(api.example.com:80)",\n        ])\n        .unwrap();\n        rt.actors\n            .get_mut(&actor_id)\n            .unwrap()\n            .install_authority_manifest(&wrong);\n        assert!(authorize_llm_client(&rt, Some(actor_id), &client).is_err());\n\n        let exact = AuthorityManifest::from_tokens([\n            "Net::TcpOut(api.example.com:443)",\n        ])\n        .unwrap();\n        rt.actors\n            .get_mut(&actor_id)\n            .unwrap()\n            .install_authority_manifest(&exact);\n        assert!(authorize_llm_client(&rt, Some(actor_id), &client).is_ok());\n    }\n\n    #[test]\n    fn malformed_llm_network_endpoint_fails_closed() {\n        let (rt, actor_id) = actor_runtime();\n        let client = OpenAiClient::with_base_url("not-a-url", "test-key", "test-model");\n        assert!(authorize_llm_client(&rt, Some(actor_id), &client).is_err());\n    }\n\n    #[test]\n    fn top_level_llm_execution_keeps_ambient_contract() {\n        let rt = Runtime::new();\n        let client = OpenAiClient::with_base_url(\n            "https://api.example.com/v1",\n            "test-key",\n            "test-model",\n        );\n        assert!(authorize_llm_client(&rt, None, &client).is_ok());\n        assert!(authorize_llm_client(&rt, Some(0), &client).is_ok());\n    }\n}\n'''
agent.write_text(text)

# ---------------------------------------------------------------------------
# General runtime HTTP: disable automatic redirects for the same exact-host
# invariant. Do not fall back to Client::new(), which would silently restore
# reqwest's default redirect policy.
# ---------------------------------------------------------------------------
replace_once(
    "src/backends/mod.rs",
    '''        let client = reqwest::Client::builder()\n            .timeout(std::time::Duration::from_secs(300))\n            .build()\n            .unwrap_or_else(|_| reqwest::Client::new());\n''',
    '''        let client = reqwest::Client::builder()\n            .redirect(reqwest::redirect::Policy::none())\n            .timeout(std::time::Duration::from_secs(300))\n            .build()\n            .expect("static runtime HTTP client configuration must be valid");\n''',
)

replace_once(
    "src/backends/mod.rs",
    '''    #[cfg(any(feature = "ai-runtime", feature = "http-client"))]\n    #[test]\n    fn test_http_provider_is_object_safe() {\n        fn accepts_http(_h: &dyn HttpProvider) {\n            // Verify trait object usage compiles (no runtime needed for type-check).\n        }\n        let provider = ReqwestHttpProvider::new();\n        accepts_http(&provider);\n    }\n''',
    '''    #[cfg(any(feature = "ai-runtime", feature = "http-client"))]\n    #[test]\n    fn test_http_provider_is_object_safe() {\n        fn accepts_http(_h: &dyn HttpProvider) {\n            // Verify trait object usage compiles (no runtime needed for type-check).\n        }\n        let provider = ReqwestHttpProvider::new();\n        accepts_http(&provider);\n    }\n\n    #[cfg(any(feature = "ai-runtime", feature = "http-client"))]\n    #[test]\n    fn test_http_provider_client_does_not_follow_redirects() {\n        use std::io::{Read, Write};\n        use std::net::TcpListener;\n\n        let listener = TcpListener::bind("127.0.0.1:0").unwrap();\n        let addr = listener.local_addr().unwrap();\n        let server = std::thread::spawn(move || {\n            let (mut stream, _) = listener.accept().unwrap();\n            let mut request = [0u8; 1024];\n            let _ = stream.read(&mut request);\n            stream\n                .write_all(\n                    b"HTTP/1.1 302 Found\\r\\nLocation: http://127.0.0.1:1/blocked\\r\\nContent-Length: 0\\r\\nConnection: close\\r\\n\\r\\n",\n                )\n                .unwrap();\n        });\n\n        let provider = ReqwestHttpProvider::new();\n        let rt = tokio::runtime::Builder::new_current_thread()\n            .enable_all()\n            .build()\n            .unwrap();\n        let response = rt\n            .block_on(provider.client.get(format!("http://{addr}/start")).send())\n            .expect("redirect response should be returned without dialing its Location");\n        assert_eq!(response.status().as_u16(), 302);\n        server.join().unwrap();\n    }\n''',
)

# Final structural assertions.
for path, needle in [
    ("crates/nulang-ai/src/client.rs", "pub enum LlmClientAuthority"),
    ("crates/nulang-ai/src/mock.rs", "LlmClientAuthority::Local"),
    ("crates/nulang-ai/src/providers/openai.rs", "LlmClientAuthority::Network"),
    ("crates/nulang-ai/src/providers/ollama.rs", "LlmClientAuthority::Network"),
    ("src/runtime/agent.rs", "network_llm_client_requires_exact_endpoint_authority"),
    ("src/backends/mod.rs", "Policy::none()"),
]:
    if needle not in Path(path).read_text():
        raise SystemExit(f"{path}: required hardening marker missing: {needle}")

print("LLM provider endpoint authority and redirect hardening applied")
