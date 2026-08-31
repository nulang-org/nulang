# v0.7 Push Instructions

## Implementation Complete Locally

Commit: `1c2cde9` at `/mnt/agents/output/nulang-impl/`
Stats: 15 files changed, 7,091 insertions(+), 31 deletions(-)

## To Push to GitHub

```bash
cd /mnt/agents/output/nulang-impl
git remote set-url origin https://YOUR_TOKEN@github.com/nulang-org/nulang.git
git push origin main
```

## What's Implemented

### New Modules (1,214 lines)
- `src/runtime/process_groups.rs` — Erlang pg-style actor groups (join/leave/members/leave_all)
- `src/runtime/registry.rs` — Actor name registry (register/whereis/registered/unregister)
- `src/runtime/timer.rs` — Hierarchical timer wheel (send_after/exit_after/kill_after/cancel)

### VM Opcode Implementations (168 lines added to vm.rs)
- `OpCode::Receive` — Pattern-matching mailbox receive with optional after timeout
- `OpCode::Monitor` — Start monitoring an actor, returns MonitorRef
- `OpCode::Demon` — Stop monitoring an actor
- `OpCode::Link` — Bidirectional fault link between actors
- `OpCode::Unlink` — Remove fault link
- `OpCode::Exit` — Typed actor exit with ExitReason propagation
- `OpCode::Yield` — Cooperative scheduler yield

### Type System (48 lines added to types.rs)
- `ExitReason` enum: Normal, Kill, Killed, Shutdown, Error, Custom
- `is_abnormal()` and `tag()` methods

### Runtime Integration (42 lines added to mod.rs)
- Timer wheel, registry, process_groups fields in Runtime struct
- Cleanup on actor exit: unregister_by_actor + leave_all

### Integration Tests (331 lines added to tests.rs)
- 24 new tests: registry(6), timers(5), pg(5), links/monitors(8)

## Files Already on Remote (correct content)
- `src/runtime/process_groups.rs` ✓
- `src/runtime/registry.rs` ✓
- `src/runtime/timer.rs` ✓
- `src/lib.rs` ✓
- `src/types.rs` ✓
- `src/runtime/actor.rs` ✓

## Files Needing Push (placeholder on remote)
- `src/vm.rs` — actual content in local commit
- `src/runtime/mod.rs` — actual content in local commit
- `src/runtime/supervisor.rs` — actual content in local commit
- `src/runtime/tests.rs` — actual content in local commit

## Strategic Documents on Remote
- `BEAM_PRIMITIVES.md` ✓ (740 lines, full adoption analysis)
- `STRATEGY.md` ✓
- `ARCHITECTURE.md` ✓
- `README2.md` ✓
- `ROADMAP.md` ✓
- `SPEC2.md` ✓
