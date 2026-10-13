# RFC 0021 — Progressive Capability Strictness

- **Status:** Draft
- **Tier:** Stable
- **Author:** dporkka
- **Created:** 2026-09-19
- **Resolved:** —
- **Language-version at effect:** —
- **Supersedes:** —
- **Superseded by:** —

## Summary

Nulang should preserve its reference-capability safety model while removing capability
annotation burden from ordinary application code. The compiler should infer the safest
useful capability when ownership is unambiguous, default actor-boundary values toward
immutable `val` data, infer `iso` ownership transfer only when the move is provable,
and explain every rejected transition in terms of actor isolation plus a concrete repair.

This RFC introduces **progressive strictness** as an opt-in package mode for Nulang 1.x.
It does not silently change the existing 1.x rule that unannotated function and behavior
parameters default to `ref`. Existing packages remain source-compatible. A future major
language version may make progressive mode the default only after migration tooling and
conformance criteria in this RFC are satisfied.

## Motivation

Nulang already has a strong capability lattice and actor-send safety rules, but its
current ergonomics expose more of that machinery than most application code should need.

The current 1.x defaults in `SPEC2.md §5.8` include:

- unannotated function and behavior parameters default to `ref`;
- literals and freshly constructed values default to `val`;
- the ambient capability-context default is `val`;
- unique ownership and linear consumption are tracked explicitly by the capability
  analyzer.

Those rules are safe, but they create two usability problems:

1. an otherwise ordinary immutable application value can acquire a `ref` parameter
   boundary and then fail when sent to an actor;
2. users can encounter a capability-lattice error before they understand the actor
   isolation invariant that produced it.

At the same time, Nulang must not solve ergonomics by weakening isolation or by making
capability inference sensitive to incidental refactoring. In particular, automatic
`iso` movement must never turn a previously non-consuming operation into a consuming
operation merely because code was rearranged.

The compiler already contains relevant foundations:

- `CapabilityAnalyzer` tracks `LinearIso`, `Linear`, and explicit `Iso` moves;
- last-use transfer inference is being developed separately and MUST remain the single
  ownership-transfer analysis rather than being duplicated here;
- structured `NuError::CapError` diagnostics and stable E04xx codes already exist;
- PR #454 begins the diagnostic portion by explaining actor-isolation failures and
  correcting misleading moved-`iso` guidance.

Progressive strictness turns those pieces into one predictable user model.

## Goals

1. Ordinary immutable application code should not require explicit capability syntax.
2. Safe actor sends should prefer `val` without weakening isolation.
3. Unique values may move implicitly only when the compiler can prove a single ownership
   transfer.
4. Inference must be stable under non-semantic refactors.
5. Explicit annotations always override inference.
6. Capability errors must explain the violated invariant and an exact repair.
7. Existing 1.x packages must keep their current semantics unless they opt in.

## Non-goals

This RFC does not:

- remove any capability from the lattice;
- make `ref`, `trn`, or `box` sendable;
- make remote `iso` values serializable;
- introduce shared mutable state between actors;
- define runtime capability checks for reference capabilities, which remain compile-time
  properties;
- duplicate the control-flow/last-use analysis used by ownership-transfer inference;
- change external-resource authority grants, which are distinct from reference
  capabilities under RFC 0019.

## Design

### 1. Package capability mode

Nulang 1.x gains a package-scoped compiler mode:

```toml
[package]
capability-mode = "progressive"
```

The absence of this key means `legacy`, preserving current 1.x behavior.

Accepted values:

```text
legacy
progressive
```

The mode is part of the semantic compilation configuration and therefore MUST contribute
to semantic/artifact identity wherever compiler options affect content identity.

The compiler CLI MAY expose an explicit override for testing and migration, but package
configuration is authoritative for published packages.

### 2. Legacy mode

Legacy mode retains the current `SPEC2.md §5.8` behavior, including unannotated
function and behavior parameters defaulting to `ref`.

Diagnostic improvements are mode-independent and may ship before the rest of this RFC.

### 3. Progressive local bindings

In progressive mode, bindings whose value is provably deeply immutable are inferred as
`val` without requiring an annotation.

Examples:

```nulang
let retries = 3
let name = "worker"
let cfg = { retries: retries, name: name }
```

All three bindings are `val` when their constituent values are deeply immutable.

The compiler MUST NOT promote a value to `val` merely because no mutation was observed
locally if its type contains actor-local mutable aliases.

### 4. Progressive parameter inference

Unannotated parameters in progressive mode are inferred from their uses subject to a
safe upper bound rather than being assigned `ref` immediately.

The inference order is constraint-based:

```text
uses requiring mutation / actor-local aliasing -> ref/trn constraint
read-only local uses                      -> val-compatible constraint
cross-actor send                          -> sendable constraint
remote send                               -> remote-sendable constraint
unique transfer                           -> iso/linear transfer constraint
explicit annotation                       -> exact user constraint
```

The compiler solves the most permissive capability that satisfies all constraints while
preserving safety. When more than one capability is valid, it MUST choose according to a
documented deterministic preference order. The initial preference order is:

```text
val -> tag -> iso -> trn -> ref -> box
```

Linear capabilities are never guessed solely to make a program type-check; they arise
from an explicitly linear binding/type or an ownership analysis that proves the required
single-use obligation.

The exact solver representation may evolve, but the observable result and diagnostics
are conformance-tested.

### 5. Actor-boundary inference

For a value sent to another actor:

- a deeply immutable value is sent as `val`;
- an already-`val` value is shared;
- an `iso` value may be moved exactly once;
- `lineariso` and `linear` preserve their existing consumption rules;
- `ref`, `trn`, and `box` are rejected.

The compiler MUST NOT silently clone mutable or unique data to satisfy sendability.

### 6. Inferred `iso` moves

Implicit movement is permitted only when the canonical ownership-transfer analysis proves
that the transfer is the final use on every relevant control-flow path.

Conceptually:

```nulang
let buffer = Buffer.new()
send worker consume(buffer)
```

may eventually be written without an explicit transfer marker when the analyzer proves
that `buffer` is dead after the send.

However:

```nulang
send worker use(buffer)
buffer.write("later")
```

MUST never change meaning because of inference. It must either remain non-consuming or
be rejected with a diagnostic that identifies the later use.

The ownership proof MUST be CFG-aware. Loops, closures, exception/effect resumptions,
suspension points, and multi-branch control flow fail closed unless the existing canonical
analysis proves the transfer.

### 7. Refactor-stability rule

Capability inference is not allowed to depend on source trivia such as:

- variable spelling;
- statement formatting;
- insertion of a semantically pure local alias;
- unrelated declarations elsewhere in a module;
- backend selection.

A refactor that preserves the typed control-flow/data-flow graph MUST preserve inferred
capabilities.

Where this cannot be guaranteed, the compiler MUST require an explicit annotation rather
than guessing.

### 8. Explicitness escape hatch

All existing explicit capability syntax remains available.

Library and systems code SHOULD continue to annotate public boundaries where the
capability is part of the API contract.

Application code SHOULD be able to rely on inference for local values and obvious actor
messages.

An explicit annotation that conflicts with inferred constraints produces an error; it is
never silently weakened.

### 9. Diagnostics contract

Every capability failure caused by actor messaging or ownership transfer SHOULD contain:

1. the capability/value involved;
2. the operation that requires a different capability;
3. **why** the operation would violate actor isolation, uniqueness, borrowing, or remote
   serialization;
4. the first relevant ownership-transfer location when available;
5. a concrete **help** action.

Examples of useful fixes include:

- freeze/project to deeply immutable `val`;
- move an `iso` value exactly once and stop using the original binding;
- keep `ref`/`trn`/`box` actor-local and send an operation/message instead;
- add the missing explicit capability annotation when inference is ambiguous.

Diagnostics MUST NOT recommend cloning a linear/unique value unless the type explicitly
supports a semantically valid copy operation that preserves the capability rules.

### 10. Tooling

The LSP SHOULD expose inferred capabilities as inlay hints on request without forcing
them into source text.

A migration command SHOULD be able to report, without rewriting by default:

- parameters whose inferred progressive capability differs from legacy `ref`;
- sends that become valid under progressive inference;
- ambiguous sites that require an explicit annotation;
- inferred ownership transfers.

The output must be stable enough for CI use.

## Tier Classification

This RFC affects the Stable capability/type-checking surface.

For Nulang 1.x:

- `legacy` remains the default;
- `progressive` is explicit opt-in;
- diagnostics may improve without changing accepted-program semantics;
- no existing package changes meaning merely by upgrading the compiler.

Making progressive mode the default is a separate major-version decision and is NOT
authorized by accepting this RFC alone.

## Backwards Compatibility

Existing 1.x source remains unchanged in legacy mode.

Opting into progressive mode may make additional safe programs compile and may infer
narrower capabilities for unannotated boundaries. Because the opt-in is package-scoped,
that change is deliberate.

Before a future major version can default to progressive mode, the project MUST provide:

1. a migration report for legacy packages;
2. a compatibility corpus covering public package APIs;
3. a documented rule for artifact/cache identity across capability modes;
4. at least one full deprecation cycle announcement;
5. conformance proving backend-independent inferred capabilities.

Explicit annotations remain source-compatible across both modes.

## Interaction with Existing Work

### Last-use transfer inference

Existing ownership-transfer work is authoritative. This RFC consumes its result; it does
not create another last-use implementation.

### RFC 0019 semantic closure

Progressive inference is subordinate to backend-invariant semantics, actor turn
isolation, and separated authority/reference-capability concepts.

### Remote execution and WASM

Progressive inference does not widen the remote-send set. Remote messages remain subject
to the serialization/capability contract of the target execution profile.

## Acceptance Criteria

The RFC is implementation-complete only when all of the following hold:

- package mode is parsed, validated, and included in semantic identity;
- legacy mode preserves the current unannotated-parameter behavior;
- progressive mode infers `val` for representative deeply immutable local/application
  values without annotations;
- progressive parameter inference is deterministic and has a documented preference
  order;
- actor sends never admit `ref`, `trn`, or `box`;
- remote sends remain restricted to the existing remote-sendable set;
- implicit `iso` transfer occurs only when the canonical last-use analysis proves it;
- a later use after an inferred move produces an ownership diagnostic with the first move
  location;
- pure alias/refactor conformance cases preserve inferred capability;
- VM/JIT/WASM frontends observe the same typed capability result before backend lowering;
- JSON, terminal, and LSP diagnostics expose equivalent error meaning;
- migration/report tooling can compare legacy and progressive results;
- documentation no longer teaches annotation-first application code.

## Required Conformance Matrix

At minimum:

| Case | Legacy | Progressive |
|---|---|---|
| immutable literal/local | existing behavior | `val` |
| unannotated read-only param | `ref` | inferred safe capability |
| local send of immutable record | current result | `val`, accepted |
| local send of `ref` mutable alias | rejected | rejected |
| one proven final `iso` send | existing rules | move allowed |
| `iso` send followed by use | rejected/explicit | rejected |
| remote `iso` send | rejected | rejected |
| remote `val` send | accepted | accepted |
| linear second use | rejected | rejected |
| semantically pure alias refactor | N/A | inference unchanged |

## Alternatives Considered

### Change the default parameter capability from `ref` to `val` immediately

Rejected. It is simple but breaks existing 1.x semantics and may reject code that
implicitly relies on mutable aliasing.

### Make every application binding `val`

Rejected. This hides important ownership distinctions and makes mutable/unique data
awkward rather than inferred.

### Infer whatever capability makes the current operation type-check

Rejected. Contextual guesswork can make semantics change under refactoring and makes
ownership transfer unpredictable.

### Require explicit capabilities everywhere

Rejected as the default application experience. Explicit capabilities remain appropriate
for public library/system boundaries.

### Automatically clone values at actor boundaries

Rejected. It obscures cost, can violate uniqueness/linearity, and changes program
semantics.

## Open Questions

1. What exact package-manifest schema/version should carry `capability-mode` so that
   package tooling and content identity remain forward-compatible?
2. Should LSP inlay hints show all inferred capabilities or only non-`val`/ownership
   transfers by default?
3. After conformance is complete, should a future major version remove legacy mode or
   retain it as an explicit compatibility profile?

None of these questions changes the load-bearing rule: 1.x semantics remain unchanged
without explicit opt-in, and inference must fail closed when ownership is ambiguous.

## Resolution

Pending.
