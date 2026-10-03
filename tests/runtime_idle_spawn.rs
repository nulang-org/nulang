use nulang::runtime::{ActorRunState, Runtime};

#[test]
fn idle_spawn_does_not_publish_ready_work() {
    let mut runtime = Runtime::new();
    let actor_id = runtime.spawn_actor(Box::new(Vec::new));

    assert_eq!(
        runtime.actors[&actor_id].run_state,
        ActorRunState::Idle,
        "an actor with no mailbox work should stay scheduler-idle after spawn"
    );
    assert!(
        runtime.scheduler.dequeue().is_none(),
        "idle spawn must not publish a ready token"
    );
}

#[test]
fn first_message_wakes_idle_spawned_actor() {
    let mut runtime = Runtime::new();
    let actor_id = runtime.spawn_actor(Box::new(Vec::new));
    runtime
        .actors
        .get_mut(&actor_id)
        .expect("spawned actor")
        .register_behavior("handle", |_actor, _args| {});

    runtime.send_message_by_id(actor_id, 0, &[]);

    assert_eq!(runtime.actors[&actor_id].run_state, ActorRunState::Queued);
    assert_eq!(runtime.scheduler.dequeue(), Some(actor_id));
}
