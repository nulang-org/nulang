from pathlib import Path

path = Path("src/stress_tests.rs")
text = path.read_text()


def replace_once(old: str, new: str, label: str) -> None:
    global text
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected one exact match, found {count}")
    text = text.replace(old, new, 1)


helper_anchor = """// ---------------------------------------------------------------------------
// Helper: TestContext
// ---------------------------------------------------------------------------
"""
helper = """fn p0_noop_behavior(_actor: &mut Actor, _args: &[Value]) {}

fn p0_register_noop(rt: &mut Runtime, actor_id: u64, behavior: &str) {
    rt.actors
        .get_mut(&actor_id)
        .expect("stress fixture actor")
        .register_behavior(behavior, p0_noop_behavior);
}

// ---------------------------------------------------------------------------
// Helper: TestContext
// ---------------------------------------------------------------------------
"""
replace_once(helper_anchor, helper, "stress helper anchor")

replace_once(
    """    let io_actor = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(12)), // 12 = "io_waiter"
            ("mode".into(), Value::int(13)), // 13 = "io_bound"
            ("quota".into(), Value::int(1)),
        ]
    }));

    // Seed workloads: send messages to each actor type
""",
    """    let io_actor = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(12)), // 12 = "io_waiter"
            ("mode".into(), Value::int(13)), // 13 = "io_bound"
            ("quota".into(), Value::int(1)),
        ]
    }));

    p0_register_noop(&mut rt, sink, "collect");
    p0_register_noop(&mut rt, cpu_actor, "compute");
    p0_register_noop(&mut rt, io_actor, "io_op");

    // Seed workloads: send messages to each actor type
""",
    "mixed workload handlers",
)

replace_once(
    """    let actor_b = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(31)), // 31 = "fair_b"
            ("quota".into(), Value::int(10)),
        ]
    }));

    const MSG_COUNT: usize = 100;
""",
    """    let actor_b = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(31)), // 31 = "fair_b"
            ("quota".into(), Value::int(10)),
        ]
    }));

    p0_register_noop(&mut rt, actor_a, "work");
    p0_register_noop(&mut rt, actor_b, "work");

    const MSG_COUNT: usize = 100;
""",
    "fairness handlers",
)

replace_once(
    """    let effect_actor = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(40)),   // 40 = "effect_resumer"
            ("effect".into(), Value::int(41)), // 41 = "SimulatedRead"
        ]
    }));

    // Start an effect on the actor
""",
    """    let effect_actor = rt.spawn_actor(Box::new(|| {
        vec![
            ("name".into(), Value::int(40)),   // 40 = "effect_resumer"
            ("effect".into(), Value::int(41)), // 41 = "SimulatedRead"
        ]
    }));

    p0_register_noop(&mut rt, effect_actor, "start_effect");
    p0_register_noop(&mut rt, effect_actor, "flood");

    // Start an effect on the actor
""",
    "effect pressure handlers",
)

replace_once(
    """    let source = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(800))]));
    let target = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(801))]));

    rt.current_actor = Some(source);
""",
    """    let source = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(800))]));
    let target = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(801))]));
    p0_register_noop(&mut rt, target, "ref");

    rt.current_actor = Some(source);
""",
    "foreign ref handler",
)

replace_once(
    """    let actor = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(900))]));

    assert!(!rt.distributed.enabled);
""",
    """    let actor = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(900))]));
    p0_register_noop(&mut rt, actor, "ping");

    assert!(!rt.distributed.enabled);
""",
    "distribution fallback handler",
)

replace_once(
    """        let id = rt.spawn_actor(Box::new(move || {
            vec![
                ("name".into(), Value::int(1000 + i as i64)),
                ("quota".into(), Value::int(5)),
            ]
        }));
        actors.push(id);
""",
    """        let id = rt.spawn_actor(Box::new(move || {
            vec![
                ("name".into(), Value::int(1000 + i as i64)),
                ("quota".into(), Value::int(5)),
            ]
        }));
        p0_register_noop(&mut rt, id, "work");
        actors.push(id);
""",
    "reduction pressure handlers",
)

replace_once(
    """    let ponger = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(101))]));

    // Pre-load the pinger's mailbox with 10,000 "pong" messages
""",
    """    let ponger = rt.spawn_actor(Box::new(|| vec![("name".into(), Value::int(101))]));
    p0_register_noop(&mut rt, pinger, "pong");

    // Pre-load the pinger's mailbox with 10,000 "pong" messages
""",
    "ping pong handler",
)

path.write_text(text)
