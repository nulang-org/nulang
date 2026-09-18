# RFC 0026: Durable Branches

- **Status:** Draft
- **Tier:** Experimental
- **Author:** Nulang Core Team
- **Created:** 2026-09-15
- **Depends on:** RFC 0005 (durable entities), RFC 0007 (event sourcing), RFC 0008 (migration contracts), RFC 0017 (time-travel debugging)

## Summary

Nulang should make counterfactual execution a first-class property of durable
entities. A durable branch captures an entity at a historical sequence, gives
the branch explicit lineage, and eventually permits that branch to execute
independently without mutating the parent timeline.

This RFC separates **branch construction** from **branch activation**.
Construction is deterministic and read-only and is implemented first.
Activation is deferred until code-version identity, workflow suspension state,
CRDT lineage, distributed ownership and provenance are explicit runtime
contracts.

## Motivation

Long-lived autonomous software needs operations that ordinary process models
do not provide safely:

- inspect a durable entity at any historical point;
- fork several candidate futures from the same state;
- compare those futures before committing an action;
- shadow-replay old history against a new code version;
- reproduce failures without re-triggering external effects;
- test migrations against production history without modifying production;
- provide human reviewers a reversible approval workflow.

These capabilities are especially useful for agent planning, beam search,
recursive improvement experiments, incident debugging and safe deployment of
entities that may live for years.

## Terminology

- **Parent**: the durable entity whose history is the source of a branch.
- **Fork sequence**: the parent sequence at which the branch diverges.
- **Branch point**: immutable reconstructed state and history through the fork
  sequence.
- **Branch lineage**: parent identity, fork sequence, code version and causal
  provenance associated with a branch.
- **Activated branch**: a branch that has received a runtime identity and may
  process new messages independently.
- **Commit**: an explicit application-level operation that uses a branch result;
  it is not an automatic merge of mutable state.

## Phase 1: deterministic branch views

The first implementation lives on the existing time-travel reconstruction
path. `fork_entity_view` returns an immutable branch point containing:

```text
branch_id
parent_actor_id
fork_sequence
parent_latest_sequence
snapshot_sequence
state
journal[<= fork_sequence]
events[<= fork_sequence]
```

Properties:

1. No user code executes while the branch is captured.
2. No external effect is performed.
3. No persistence is mutated.
4. The parent remains live and unchanged.
5. The same durable store and fork sequence produce the same reconstructed
   event-sourced state.
6. Branch JSON explicitly reports that the branch is not executable.

`diff_states` provides deterministic field-level comparison and
`diff_branch_from_parent_head` compares a branch point with its parent's
current head.

## Phase 2: version-pinned branch manifests

Before activation, each branch MUST gain a persisted manifest:

```text
BranchManifest {
  branch_id
  parent_entity_id
  fork_sequence
  code_hash
  schema_version
  runtime_version
  journal_format_version
  created_at_hlc
  created_by_principal
  capability_envelope
  provenance_hash
}
```

The code hash should use Nulang's content-addressed artifact identity. A branch
must never silently execute against whatever code happens to be deployed when
it resumes.

## Phase 3: shadow replay

A version-pinned branch may replay historical inputs under a candidate code
version in an effect-isolated environment. External effects MUST be handled by
recorded results or shadow handlers.

The runtime should produce:

```text
state diff
effect diff
output diff
resource/cost diff
migration diagnostics
```

This becomes the default validation path for entity upgrades.

## Phase 4: executable branches

An executable branch receives a distinct durable identity and may append new
history after the fork sequence.

Activation MUST establish:

- a code version and schema version;
- ownership/placement in the cluster;
- a valid workflow suspension representation if applicable;
- a valid CRDT branch policy if CRDT state exists;
- a fresh capability envelope or an explicitly inherited restricted envelope;
- cryptographically attributable lineage in Nulang Cloud.

An activated branch MUST NOT share mutable local state with its parent.

## Workflow semantics

Workflow state cannot be inferred solely from entity field values. Activation
therefore requires the workflow event log and suspension state to be replayable
through the fork sequence. Until that invariant is implemented, workflow
branches remain read-only views.

## CRDT semantics

CRDTs require explicit branch semantics because copying a CRDT state and later
merging it into the same replica set may duplicate or violate causal identity.
The runtime MUST NOT activate a CRDT-bearing branch until it can assign fresh
replica identity and define merge provenance.

Recommended default: a branch receives fresh CRDT replica identity. Committing
a branch means applying explicit domain events or commands to the parent, not
blindly merging internal CRDT replicas.

## Effects and safety

A replayable branch region must not issue irreversible effects directly.
Eventually the compiler should reject or require explicit handling for:

```text
non-idempotent external effect in replayable branch
irreversible effect in rollback-capable region
unrecorded nondeterministic effect during shadow replay
```

Effect handlers may provide one of:

- recorded replay result;
- deterministic simulator;
- shadow/no-op implementation;
- explicitly authorized live execution.

Live execution should be opt-in and visibly dangerous.

## Commit semantics

Nulang MUST NOT define `commit(branch)` as generic state merge. Arbitrary actor
state is not necessarily mergeable.

Instead, applications commit through typed commands/events, for example:

```nulang
let candidate = branch.best_plan
send parent apply_plan(candidate)
```

For domains with algebraically mergeable state, a type-specific merge policy
may be provided explicitly.

## Cloud control plane

Nulang Cloud should expose a branch timeline with:

```text
inspect
fork
compare
shadow replay
promote result
discard
export provenance
```

Every branch operation should be written to the tamper-evident execution audit
trail and later signed with workload/principal identity.

## Non-goals

- Transparent generic merging of arbitrary branches.
- Cluster-wide rewind without a distributed snapshot protocol.
- Re-executing unjournaled external effects during debugger rewind.
- Treating a branch as executable before version, workflow and CRDT contracts
  are explicit.

## Rollout

1. **Implemented in this change:** immutable branch views and deterministic
   state diffs in `dap::rewind`.
2. Persist `BranchManifest` with content-addressed code identity.
3. Add shadow replay with effect isolation.
4. Integrate migration contracts and code-version pinning.
5. Add runtime activation for ordinary durable/event-sourced entities.
6. Add workflow-aware activation.
7. Add CRDT-aware activation with fresh replica identity.
8. Add Cloud timeline/provenance UI and APIs.

## Competitive rationale

Several durable runtimes expose rewind, replay, worker/version management or
fork-like functionality independently. Nulang can make these operations more
powerful because the language, effect system, durable entity model and runtime
share one semantic layer. Durable branches should therefore be designed as a
core lifecycle primitive, not as debugger-only UI sugar.