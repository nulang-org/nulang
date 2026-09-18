from pathlib import Path

path = Path("src/runtime/tests.rs")
text = path.read_text()

old = '''fn test_distributed_remote_address_local_fallback() {
    // A REMOTE address whose node is the local node (or a runtime with
    // distributed disabled) must deliver locally instead of silently
    // dropping — the single-node case of the SPEC2 known-issue list
    // (send/ask remote). `Runtime::send_distributed` resolves through
    // the distribution wrapper: distributed disabled → local delivery.
    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![("val".to_string(), Value::int(0))]));

    // Distributed is disabled by default: a remote address still delivers.
'''
new = '''fn test_distributed_remote_address_local_fallback() {
    // A REMOTE address whose node is the local node (or a runtime with
    // distributed disabled) must deliver locally instead of silently
    // dropping — the single-node case of the SPEC2 known-issue list
    // (send/ask remote). `Runtime::send_distributed` resolves through
    // the distribution wrapper: distributed disabled → local delivery.
    // Register the named behavior explicitly so this regression isolates
    // address fallback rather than relying on the removed unknown-name → 0 alias.
    fn noop(_actor: &mut Actor, _args: &[Value]) {}

    let mut rt = Runtime::new();
    let actor_id = rt.spawn_actor(Box::new(|| vec![("val".to_string(), Value::int(0))]));
    rt.actors
        .get_mut(&actor_id)
        .expect("spawned actor")
        .register_behavior("test", noop);

    // Distributed is disabled by default: a remote address still delivers.
'''

count = text.count(old)
if count != 1:
    raise SystemExit(f"remote fallback fixture: expected one exact match, found {count}")

text = text.replace(old, new, 1)


# Scheduler/statistics fixtures must name real handlers instead of relying on
# the removed unknown-name -> behavior-0 alias.
old = '''    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    // Drain the spawn-time queue entries (both enqueued at Normal).
'''
new = '''    fn noop(_actor: &mut Actor, _args: &[Value]) {}

    let a = rt.spawn_actor(Box::new(|| vec![]));
    let b = rt.spawn_actor(Box::new(|| vec![]));
    rt.actors.get_mut(&a).unwrap().register_behavior("noop", noop);
    rt.actors.get_mut(&b).unwrap().register_behavior("noop", noop);
    // Drain the spawn-time queue entries (both enqueued at Normal).
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"priority fixture: expected one exact match, found {count}")
text = text.replace(old, new, 1)

old = '''    let a1 = rt.spawn_actor(Box::new(|| vec![("counter".to_string(), Value::int(0))]));
    let a2 = rt.spawn_actor(Box::new(|| vec![("counter".to_string(), Value::int(0))]));
    rt.send_message(a1, "add", &[Value::int(10)]);
'''
new = '''    fn noop(_actor: &mut Actor, _args: &[Value]) {}

    let a1 = rt.spawn_actor(Box::new(|| vec![("counter".to_string(), Value::int(0))]));
    let a2 = rt.spawn_actor(Box::new(|| vec![("counter".to_string(), Value::int(0))]));
    rt.actors.get_mut(&a1).unwrap().register_behavior("add", noop);
    rt.actors.get_mut(&a2).unwrap().register_behavior("add", noop);
    rt.send_message(a1, "add", &[Value::int(10)]);
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"scheduler stats fixture: expected one exact match, found {count}")
text = text.replace(old, new, 1)

if "fn p0_cross_shard_named_send_resolves_on_owner_and_unknown_fails_closed()" in text:
    raise SystemExit("cross-shard P0 test already present")
text += r'''

#[test]
fn p0_cross_shard_named_send_resolves_on_owner_and_unknown_fails_closed() {
    fn increment(actor: &mut Actor, args: &[Value]) {
        let n = actor
            .get_state_field("count")
            .and_then(|v| v.as_int())
            .unwrap_or(0);
        let by = args.first().and_then(|v| v.as_int()).unwrap_or(0);
        actor.set_state_field("count", Value::int(n + by));
    }

    let mut shards = Runtime::new_sharded(2);
    let mut target =
        shards[1].spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
    while target % 2 != 1 {
        target =
            shards[1].spawn_actor(Box::new(|| vec![("count".to_string(), Value::int(0))]));
    }
    shards[1]
        .actors
        .get_mut(&target)
        .unwrap()
        .register_behavior("inc", increment);

    shards[0].send_message(target, "missing", &[Value::int(7)]);
    shards[1].run_scheduler();
    assert_eq!(
        shards[1]
            .actors
            .get(&target)
            .unwrap()
            .get_state_field("count")
            .and_then(|v| v.as_int()),
        Some(0),
        "unknown cross-shard names must not alias behavior 0"
    );

    shards[0].send_message(target, "inc", &[Value::int(3)]);
    shards[1].run_scheduler();
    assert_eq!(
        shards[1]
            .actors
            .get(&target)
            .unwrap()
            .get_state_field("count")
            .and_then(|v| v.as_int()),
        Some(3),
        "valid cross-shard names must resolve on the owning shard"
    );
}
'''

path.write_text(text)
