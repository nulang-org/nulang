# NLAP v1 — NuLang Agent Protocol

Version: 1.0.0

Shared between NuLang Agent Runtime (OSS) and NuLang AI Cloud (managed).

- `domain.schema.json` — Goal, Task, Conversation, Artifact, Evidence, Budget
- `events.schema.json` — NLAP event envelope

NLAP v1 is additive within the major version: runtimes should ignore unknown fields and preserve identifiers when bridging tasks between local, cloud, and external control planes.
