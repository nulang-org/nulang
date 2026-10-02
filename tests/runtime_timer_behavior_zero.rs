use std::cell::RefCell;
use std::rc::Rc;

use nulang::bytecode::Constant;
use nulang::runtime::{Actor, Runtime, RuntimeVmCallbacks};
use nulang::vm::{ActorVmCallbacks, Value};

fn noop(_actor: &mut Actor, _args: &[Value]) {}

fn runtime_with_behaviors(names: &[&str]) -> (Rc<RefCell<Runtime>>, u64) {
    let runtime = Rc::new(RefCell::new(Runtime::new()));
    let actor_id = runtime.borrow_mut().spawn_actor(Box::new(|| Vec::new()));

    {
        let mut rt = runtime.borrow_mut();
        let actor = rt.actors.get_mut(&actor_id).expect("spawned actor");
        for name in names {
            actor.register_behavior(*name, noop);
        }
        rt.current_actor = Some(actor_id);
    }

    (runtime, actor_id)
}

fn schedule_after(runtime: Rc<RefCell<Runtime>>, callback_name: &str) {
    let constants = [Constant::String(callback_name.to_string())];
    let regs = [Value::int(60_000), Value::string(0)];
    let mut callbacks = RuntimeVmCallbacks::new(runtime);

    let _ = callbacks.perform_builtin_effect("Timer", Some("after"), &constants, &regs);
}

#[test]
fn timer_after_schedules_callback_at_behavior_zero() {
    let (runtime, actor_id) = runtime_with_behaviors(&["first"]);
    assert_eq!(runtime.borrow().behavior_id_for(actor_id, "first"), Some(0));

    schedule_after(runtime.clone(), "first");

    assert_eq!(
        runtime.borrow().timer_wheel.len(),
        1,
        "behavior id 0 is a valid callback and must not be treated as missing"
    );
}

#[test]
fn timer_after_unknown_callback_does_not_alias_behavior_zero() {
    let (runtime, actor_id) = runtime_with_behaviors(&["first"]);
    assert_eq!(runtime.borrow().behavior_id_for(actor_id, "first"), Some(0));
    assert_eq!(runtime.borrow().behavior_id_for(actor_id, "missing"), None);

    schedule_after(runtime.clone(), "missing");

    assert!(
        runtime.borrow().timer_wheel.is_empty(),
        "an unknown callback must remain unresolved instead of aliasing behavior id 0"
    );
}

#[test]
fn timer_after_still_schedules_nonzero_callback() {
    let (runtime, actor_id) = runtime_with_behaviors(&["first", "second"]);
    assert_eq!(
        runtime.borrow().behavior_id_for(actor_id, "second"),
        Some(1)
    );

    schedule_after(runtime.clone(), "second");

    assert_eq!(runtime.borrow().timer_wheel.len(), 1);
}
