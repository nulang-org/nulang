# Mission Runtime

Nulang missions are an **AI/runtime SDK abstraction**, not a Nulang language keyword.

This boundary follows the durable-computation direction in RFC 0004 and RFC 0005: the language provides long-lived entities, actors, effects, capabilities, and durability; rapidly evolving agent orchestration policy stays in libraries and Nulang Cloud.

## Contract

`nulang-ai-core::MissionSpec` wraps a durable `Goal` with execution policy:

- a maximum dollar budget,
- a maximum number of materialized tasks,
- bounded parallelism,
- a wall-clock deadline budget,
- an optional aggregate token ceiling,
- capability requirements,
- human-approval policy, and
- verification/repair policy.

The contract is serializable so the same mission can move between local and cloud runtimes without changing the programming-language surface.

## Planning

Managers may implement specialized planning, but `Manager::plan_mission` applies common hard constraints after decomposition:

1. The effective task budget cannot exceed the mission budget.
2. The task graph is truncated to `max_tasks`.
3. Aggregate planned task spend is scaled down when it exceeds the mission ceiling.
4. Individual task timeouts are clamped to the mission wall-clock budget.
5. Mission-required capabilities are propagated to each planned task.
6. Goal success criteria become task acceptance criteria when the manager has not supplied stronger ones.

This is intentionally policy enforcement rather than prompt guidance.

## Approval boundary

`ApprovalPolicy` supports unrestricted, always-gated, and capability-gated execution. The default policy treats the following capabilities as privileged:

- `deploy.production`
- `secrets.write`
- `billing.write`

Nulang Cloud is responsible for enforcing this policy before side effects occur. A prompt telling an agent not to deploy is not a security boundary; a missing capability or unresolved approval is.

## Verification

`VerificationPolicy` describes the checks required before a mission may be considered complete. The initial contract records:

- whether verification is required,
- the minimum number of successful checks, and
- the maximum repair attempts.

Concrete verifiers remain runtime concerns. They may include deterministic tests, schema assertions, browser assertions, independent model judges, policy checks, or human approval.

## Intended execution model

```text
intent
  -> durable Goal
  -> MissionSpec
  -> manager decomposition
  -> capability / approval gate
  -> dependency-ready task waves
  -> workers / environments
  -> verification
  -> completed artifact
```

Future worker-fabric and environment APIs should consume `MissionSpec` rather than introduce parallel mission models. That keeps local execution, Nulang Cloud, and future self-hosted workers on one portable contract.
