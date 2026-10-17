# Immutable agent release contract (v1)

Status: **additive pre-admission contract; not enforced by production deployments**.

Nulang creates and validates a sealed agent-release envelope. Nulang Cloud
validates the same envelope before future admission integration. A release
identity is **not** a signature, a tenant authorization decision, a secret,
a permission grant, or evidence that referenced assets exist.

## JSON envelope

```json
{
  "definition": {
    "schema": "nulang.agent-release/v1",
    "agent": "research-agent",
    "version": "1.0.0",
    "artifact_id": "<64 lowercase hex characters from the compiler>",
    "prompt": "blake3:<64 lowercase hex characters>",
    "policy": "blake3:<64 lowercase hex characters>",
    "model": {
      "provider": "mistral",
      "model": "large",
      "revision": "2026-10-17"
    },
    "skills": [
      {"name": "summarize", "digest": "blake3:<64 lowercase hex characters>"}
    ],
    "tools": [
      {"name": "search", "digest": "blake3:<64 lowercase hex characters>"}
    ]
  },
  "release_id": "blake3:<calculated 64 lowercase hex characters>"
}
```

Replace example placeholders with real compiler identity and exact asset
content hashes. Model revision MUST refer to an immutable provider revision,
not a floating alias. Validating the revision string does not prove provider
immutability; trusted Cloud policy must independently establish that.

## Canonical release ID

```text
BLAKE3(
  ASCII("nulang.agent-release.v1") || 0x00 ||
  LP(schema) || LP(agent) || LP(version) || LP(artifact_id) ||
  LP(prompt) || LP(policy) ||
  LP(model.provider) || LP(model.model) || LP(model.revision) ||
  u32le(number_of_skills) ||
    for each skill by ascending name: LP(name) || LP(digest) ||
  u32le(number_of_tools) ||
    for each tool by ascending name: LP(name) || LP(digest)
)
```

`LP(s)` is `u32le(UTF8_byte_length(s)) || UTF8(s)`. The output is the
literal `blake3:` plus the lowercase 64-character hex digest. Names and
versions are bounded ASCII identifiers; both asset lists have at most 64
entries and must be strictly sorted without duplicates. The envelope is
limited to 64 KiB. Schema and all JSON keys are exact and unknown fields
are rejected. Whitespace or JSON object-key order does not change the ID.

Prompt, policy, skill and tool digests are **raw BLAKE3 of exact asset bytes**,
not the release identity algorithm. The `artifact_id` is the Nulang
compiler-produced artifact identity, not a substitute for verifying the
actual compiled executable and Behavior Manifest.

## Security and admission boundaries

1. Fetch immutable asset bytes using **trusted** tenant-scoped storage
   credentials and verify every declared content digest with `verify_asset`.
2. Independently verify the compiler's executable artifact digest, compiler
   artifact identity and Behavior Manifest admission rules.
3. Compare the computed `release_id` to the **trusted** release ID bound to
   the tenant's deployment or approval record. A client-supplied, self-sealed
   ID alone is not evidence of authenticity.
4. Apply Cloud's existing deterministic capability/authority checks. Declared
   policy content cannot widen capabilities; a model or LLM guard cannot
   authorize effects.
5. Journal the verified release ID with each durable invocation. Retain old
   assets for replay and distinguish model configuration from model outputs.

For this PR, only the pure seal/parse/verify/content-digest primitives are
implemented in both codebases. Tenant binding, storage fetch, durable
invocation journal integration and live deploy promotion are deliberately
separate work and **must not** be inferred from this contract.

## Tests

Nulang: `cargo test --test agent_release_contract` and
`cargo test --test agent_release_cli`.

Nulang Cloud: `cargo test -p nlc-deploy --test agent_release_contract`.
