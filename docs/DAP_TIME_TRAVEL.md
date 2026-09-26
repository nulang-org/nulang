# DAP Time-Travel Rewind (Wave E3)

Dev/staging-only time-travel debugging for durable entities: rewind an
actor/entity to message #N by restoring its snapshot and replaying recorded
events `1..=N`, then step forward again. Single-node, single-entity only;
cluster-wide vector-clock rewind is out of scope.

The rewind layer also exposes immutable **durable branch views**. A branch
captures reconstructed state plus explicit parent/fork lineage without
mutating the live entity. This is the first step toward executable durable
branches for speculative agents and shadow migrations while keeping the
current debugger semantics deterministic and safe.

## Enabling

Rewind is gated on the durable store: it is active only when
`NULANG_STORE_PATH` is set or `.nulang/store/` exists (same resolution as
the CLI `--store` flag). The adapter then advertises
`supportsReverseContinueRequest` and `supportsStepBack` in `initialize`.

## Requests (extension arguments)

- `reverseContinue` `{actorId, targetSequence}` — reconstruct the entity's
  state as of message `targetSequence` (clamped to the latest recorded
  sequence) and remember that position.
- `stepBack` `{actorId}` — rewind one message from the current position
  (default: the latest).
- `nulangStepForward` `{actorId}` — step forward one message from the
  current position by replaying the next recorded events (no-op at the head).

Each response body carries `sequence`, `latestSequence`,
`snapshotSequence`, the reconstructed `state` map, and the `journal` of
messages delivered up to that point.

## Durable branch views

`dap::rewind::fork_entity_view(store, actor_id, target_sequence, branch_id)`
captures an immutable branch point containing:

- `branch_id` — caller-defined branch identity;
- `parent_actor_id` — the source entity;
- `fork_sequence` — the exact historical divergence point;
- `parent_latest_sequence` — the parent head when the branch was captured;
- `snapshot_sequence` — the reconstruction base;
- reconstructed state, journal entries and event-sourcing entries through the
  fork point.

`diff_branch_from_parent_head` compares the branch point with the current
parent head, and `diff_states` provides the lower-level deterministic
field-by-field diff operation.

Branch capture is intentionally read-only in this wave. A branch is marked
`executable: false` in its JSON representation. Activating a historical
branch as a live entity is deferred until the runtime has explicit contracts
for code-version identity, workflow suspension state, CRDT lineage,
distributed ownership and branch provenance. That avoids creating a
"fork" operation that appears safe while silently losing state that rewind
cannot currently reconstruct.

## Semantics and determinism

Rewind never re-executes behavior bytecode. The base is the latest snapshot
with `sequence <= N`; `event_sourced` fields are then overlaid from the
event log using each event's **recorded post-apply value** (`EventEntry.
value`). Replay is therefore a pure function of the log — deterministic by
construction (SPEC2 §9.7). Non-deterministic effects in behavior bodies
must come from journaled effects (SPEC2 §9.7a, added separately); the
journaled event values are exactly that journal for `event_sourced` fields.

Durable branch views inherit exactly these semantics. Capturing a branch
performs no user-code execution, no external effects and no persistence
writes. The parent entity is unchanged.

## Limitations

- `durable` (non-event-sourced) fields are only known at snapshot
  granularity; intermediate values between snapshots are not
  reconstructible without re-execution and are reported from the base
  snapshot (or declared defaults when no snapshot precedes N).
- Rewind and branch views do not touch the live debuggee VM; they inspect
  persisted state.
- Branch views are not executable actors yet.
- Workflow suspension state and CRDT state are not reconstructed beyond the
  information represented by the selected snapshot/event history.
- The event-sourcing/journal format is unchanged (backward compatible).