# DAP Time-Travel Rewind (Wave E3)

Dev/staging-only time-travel debugging for durable entities: rewind an
actor/entity to message #N by restoring its snapshot and replaying recorded
events `1..=N`, then step forward again. Single-node, single-entity only;
cluster-wide vector-clock rewind is out of scope.

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

## Semantics and determinism

Rewind never re-executes behavior bytecode. The base is the latest snapshot
with `sequence <= N`; `event_sourced` fields are then overlaid from the
event log using each event's **recorded post-apply value** (`EventEntry.
value`). Replay is therefore a pure function of the log — deterministic by
construction (SPEC2 §9.7). Non-deterministic effects in behavior bodies
must come from journaled effects (SPEC2 §9.7a, added separately); the
journaled event values are exactly that journal for `event_sourced` fields.

## Limitations

- `durable` (non-event-sourced) fields are only known at snapshot
  granularity; intermediate values between snapshots are not
  reconstructible without re-execution and are reported from the base
  snapshot (or declared defaults when no snapshot precedes N).
- Rewind does not touch the live debuggee VM; it inspects persisted state.
- The event-sourcing/journal format is unchanged (backward compatible).
