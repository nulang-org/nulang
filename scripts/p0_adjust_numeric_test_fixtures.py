from pathlib import Path

path = Path("src/runtime/tests.rs")
text = path.read_text()


def replace_in_test(name: str, old: str, new: str) -> None:
    global text
    marker = f"fn {name}() {{\n"
    start = text.index(marker)
    end = text.find("\n#[test]\n", start + len(marker))
    if end < 0:
        end = len(text)
    segment = text[start:end]
    count = segment.count(old)
    if count != 1:
        raise SystemExit(f"{name}: expected one fixture anchor, found {count}")
    segment = segment.replace(old, new, 1)
    text = text[:start] + segment + text[end:]


helper = r"""
fn p0_numeric_test_noop(_actor: &mut Actor, _args: &[Value]) {}

fn p0_register_numeric_zero(rt: &mut Runtime, actor_id: u64) {
    rt.actors
        .get_mut(&actor_id)
        .expect("numeric-delivery fixture actor")
        .register_behavior("__p0_numeric_noop", p0_numeric_test_noop);
}
"""
if "fn p0_register_numeric_zero(" in text:
    raise SystemExit("numeric fixture helper already present")
text += "\n" + helper

single_b_tests = [
    "test_send_carries_current_trace_span",
    "test_delivery_establishes_child_context_and_inherits",
    "test_supervised_child_restart_retires_heap_with_foreign_refs",
    "test_cycle_detector_registers_real_cross_actor_ref",
    "test_cycle_detector_accumulates_edge_ref_count",
    "test_cross_actor_send_foreign_count_lifecycle",
    "test_run_scheduler_pumps_gc",
    "test_exiting_sender_heap_retired_until_refs_drain",
    "test_receiver_hold_survives_sender_drop_until_release",
]
for name in single_b_tests:
    replace_in_test(
        name,
        "    let b = rt.spawn_actor(Box::new(|| vec![]));\n",
        "    let b = rt.spawn_actor(Box::new(|| vec![]));\n"
        "    p0_register_numeric_zero(&mut rt, b);\n",
    )

replace_in_test(
    "test_forwarding_received_reference_uses_true_owner",
    "    let b = rt.spawn_actor(Box::new(|| vec![]));\n"
    "    let c = rt.spawn_actor(Box::new(|| vec![]));\n",
    "    let b = rt.spawn_actor(Box::new(|| vec![]));\n"
    "    let c = rt.spawn_actor(Box::new(|| vec![]));\n"
    "    p0_register_numeric_zero(&mut rt, b);\n"
    "    p0_register_numeric_zero(&mut rt, c);\n",
)

for name in [
    "test_object_ref_send_same_shard_records_hold",
    "test_object_ref_released_on_actor_exit",
]:
    replace_in_test(
        name,
        "    let receiver = rt.spawn_actor(Box::new(|| vec![]));\n",
        "    let receiver = rt.spawn_actor(Box::new(|| vec![]));\n"
        "    p0_register_numeric_zero(&mut rt, receiver);\n",
    )

replace_in_test(
    "test_object_ref_cross_shard_copies_bytes",
    "    assert_eq!(source_shard, 0);\n"
    "    assert_eq!(target_shard, 1);\n",
    "    assert_eq!(source_shard, 0);\n"
    "    assert_eq!(target_shard, 1);\n"
    "    p0_register_numeric_zero(&mut shards[target_shard], b);\n",
)

path.write_text(text)
