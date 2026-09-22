use crate::{
    AppendOutcome, SignalId, SignalWaitId, TimerId, WorkflowEngineError, WorkflowEvent,
    WorkflowHistory, WorkflowId, WorkflowRuntime,
};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalNotifyOutcome {
    Recorded,
    AlreadyRecorded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalProgress {
    Waiting,
    Received {
        signal_id: SignalId,
        payload: Vec<u8>,
    },
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DurableSignalExecutor;

impl DurableSignalExecutor {
    pub fn notify<R: WorkflowRuntime>(
        &self,
        runtime: &mut R,
        workflow_id: &WorkflowId,
        signal_id: SignalId,
        name: impl Into<String>,
        payload: impl Into<Vec<u8>>,
    ) -> Result<SignalNotifyOutcome, WorkflowEngineError<R::Error>> {
        let name = name.into();
        let payload = payload.into();
        if signal_id.as_str().is_empty() {
            return Err(WorkflowEngineError::HistoryConflict(
                "signal id must not be empty".into(),
            ));
        }
        if name.is_empty() {
            return Err(WorkflowEngineError::HistoryConflict(
                "signal name must not be empty".into(),
            ));
        }

        let history = runtime
            .load_history(workflow_id)
            .map_err(WorkflowEngineError::Runtime)?;
        let mut found = false;
        for event in &history.events {
            if let WorkflowEvent::SignalReceived {
                signal_id: existing_id,
                name: existing_name,
                payload: existing_payload,
            } = event
            {
                if existing_id == &signal_id {
                    if found {
                        return Err(WorkflowEngineError::HistoryConflict(format!(
                            "signal {} has multiple receive records",
                            signal_id
                        )));
                    }
                    found = true;
                    if existing_name != &name || existing_payload != &payload {
                        return Err(WorkflowEngineError::HistoryConflict(format!(
                            "signal {} was replayed with different content",
                            signal_id
                        )));
                    }
                }
            }
        }

        if found {
            return Ok(SignalNotifyOutcome::AlreadyRecorded);
        }

        append(
            runtime,
            workflow_id,
            history.revision,
            WorkflowEvent::SignalReceived {
                signal_id,
                name,
                payload,
            },
        )?;
        Ok(SignalNotifyOutcome::Recorded)
    }

    pub fn receive<R: WorkflowRuntime>(
        &self,
        runtime: &mut R,
        workflow_id: &WorkflowId,
        name: &str,
        occurrence: u32,
    ) -> Result<SignalProgress, WorkflowEngineError<R::Error>> {
        if name.is_empty() {
            return Err(WorkflowEngineError::HistoryConflict(
                "signal name must not be empty".into(),
            ));
        }
        let wait_id = SignalWaitId::derive(workflow_id, name, occurrence);
        let history = runtime
            .load_history(workflow_id)
            .map_err(WorkflowEngineError::Runtime)?;
        let state = inspect_signals(&history, &wait_id, name)?;

        if let Some((signal_id, payload)) = state.delivered {
            return Ok(SignalProgress::Received { signal_id, payload });
        }

        let Some((signal_id, payload)) = state.available else {
            return Ok(SignalProgress::Waiting);
        };
        append(
            runtime,
            workflow_id,
            history.revision,
            WorkflowEvent::SignalDelivered {
                wait_id,
                signal_id: signal_id.clone(),
                name: name.to_owned(),
                payload: payload.clone(),
            },
        )?;
        Ok(SignalProgress::Received { signal_id, payload })
    }
}

struct SignalState {
    delivered: Option<(SignalId, Vec<u8>)>,
    available: Option<(SignalId, Vec<u8>)>,
}

fn inspect_signals<E>(
    history: &WorkflowHistory,
    requested_wait_id: &SignalWaitId,
    requested_name: &str,
) -> Result<SignalState, WorkflowEngineError<E>> {
    let mut received: HashMap<SignalId, (String, Vec<u8>)> = HashMap::new();
    let mut received_order = Vec::new();
    let mut delivered_signals = HashSet::new();
    let mut delivered_waits = HashSet::new();
    let mut delivered = None;

    for event in &history.events {
        match event {
            WorkflowEvent::SignalReceived {
                signal_id,
                name,
                payload,
            } => {
                if received.contains_key(signal_id) {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "signal {} has multiple receive records",
                        signal_id
                    )));
                }
                received.insert(signal_id.clone(), (name.clone(), payload.clone()));
                received_order.push(signal_id.clone());
            }
            WorkflowEvent::SignalDelivered {
                wait_id,
                signal_id,
                name,
                payload,
            } => {
                if !delivered_waits.insert(*wait_id) {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "signal wait {} has multiple delivery records",
                        wait_id
                    )));
                }
                if !delivered_signals.insert(signal_id.clone()) {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "signal {} was delivered more than once",
                        signal_id
                    )));
                }
                let Some((received_name, received_payload)) = received.get(signal_id) else {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "signal {} was delivered before it was received",
                        signal_id
                    )));
                };
                if received_name != name || received_payload != payload {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "signal {} delivery does not match its receive record",
                        signal_id
                    )));
                }
                if wait_id == requested_wait_id {
                    if name != requested_name {
                        return Err(WorkflowEngineError::HistoryConflict(format!(
                            "signal wait {} changed names during replay",
                            wait_id
                        )));
                    }
                    delivered = Some((signal_id.clone(), payload.clone()));
                }
            }
            _ => {}
        }
    }

    let available = if delivered.is_none() {
        received_order.into_iter().find_map(|signal_id| {
            if delivered_signals.contains(&signal_id) {
                return None;
            }
            let (name, payload) = received.get(&signal_id)?;
            (name == requested_name).then(|| (signal_id, payload.clone()))
        })
    } else {
        None
    };

    Ok(SignalState {
        delivered,
        available,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimerArmRequest {
    pub workflow_id: WorkflowId,
    pub timer_id: TimerId,
    pub name: String,
    pub occurrence: u32,
    pub fire_at_millis: u64,
}

pub trait WorkflowTimerRuntime: WorkflowRuntime {
    /// Arm or re-arm a durable wakeup. Implementations must be idempotent by
    /// `timer_id`; recovery may call this repeatedly for the same timer.
    fn arm_timer(&mut self, request: TimerArmRequest) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerProgress {
    Waiting {
        timer_id: TimerId,
        fire_at_millis: u64,
    },
    Fired {
        timer_id: TimerId,
        fire_at_millis: u64,
    },
}

#[derive(Debug, Default, Clone, Copy)]
pub struct DurableTimerExecutor;

impl DurableTimerExecutor {
    pub fn wait<R: WorkflowTimerRuntime>(
        &self,
        runtime: &mut R,
        workflow_id: &WorkflowId,
        name: &str,
        occurrence: u32,
        delay_millis: u64,
    ) -> Result<TimerProgress, WorkflowEngineError<R::Error>> {
        if name.is_empty() {
            return Err(WorkflowEngineError::HistoryConflict(
                "timer name must not be empty".into(),
            ));
        }
        let timer_id = TimerId::derive(workflow_id, name, occurrence);
        let mut history = runtime
            .load_history(workflow_id)
            .map_err(WorkflowEngineError::Runtime)?;
        let mut state = inspect_timer(
            &history,
            &timer_id,
            name,
            occurrence,
            delay_millis,
        )?;

        if state.fired {
            return Ok(TimerProgress::Fired {
                timer_id,
                fire_at_millis: state
                    .fire_at_millis
                    .expect("fired timer must have a schedule"),
            });
        }

        if state.fire_at_millis.is_none() {
            let scheduled_at_millis = runtime.now_millis();
            let fire_at_millis = scheduled_at_millis.saturating_add(delay_millis);
            history.revision = append(
                runtime,
                workflow_id,
                history.revision,
                WorkflowEvent::TimerScheduled {
                    timer_id,
                    name: name.to_owned(),
                    occurrence,
                    delay_millis,
                    scheduled_at_millis,
                    fire_at_millis,
                },
            )?;
            state.fire_at_millis = Some(fire_at_millis);
        }

        let fire_at_millis = state
            .fire_at_millis
            .expect("timer schedule exists after initialization");
        if runtime.now_millis() >= fire_at_millis {
            append(
                runtime,
                workflow_id,
                history.revision,
                WorkflowEvent::TimerFired {
                    timer_id,
                    fire_at_millis,
                },
            )?;
            return Ok(TimerProgress::Fired {
                timer_id,
                fire_at_millis,
            });
        }

        runtime
            .arm_timer(TimerArmRequest {
                workflow_id: workflow_id.clone(),
                timer_id,
                name: name.to_owned(),
                occurrence,
                fire_at_millis,
            })
            .map_err(WorkflowEngineError::Runtime)?;
        Ok(TimerProgress::Waiting {
            timer_id,
            fire_at_millis,
        })
    }
}

struct TimerState {
    fire_at_millis: Option<u64>,
    fired: bool,
}

fn inspect_timer<E>(
    history: &WorkflowHistory,
    timer_id: &TimerId,
    name: &str,
    occurrence: u32,
    delay_millis: u64,
) -> Result<TimerState, WorkflowEngineError<E>> {
    let mut fire_at_millis = None;
    let mut fired = false;

    for event in &history.events {
        match event {
            WorkflowEvent::TimerScheduled {
                timer_id: id,
                name: stored_name,
                occurrence: stored_occurrence,
                delay_millis: stored_delay,
                scheduled_at_millis,
                fire_at_millis: stored_fire_at,
            } if id == timer_id => {
                if fire_at_millis.is_some() {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "timer {} has multiple schedule records",
                        timer_id
                    )));
                }
                if stored_name != name
                    || *stored_occurrence != occurrence
                    || *stored_delay != delay_millis
                    || scheduled_at_millis.saturating_add(*stored_delay) != *stored_fire_at
                {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "timer {} schedule changed during replay",
                        timer_id
                    )));
                }
                fire_at_millis = Some(*stored_fire_at);
            }
            WorkflowEvent::TimerFired {
                timer_id: id,
                fire_at_millis: fired_at,
            } if id == timer_id => {
                let Some(scheduled_fire_at) = fire_at_millis else {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "timer {} fired before it was scheduled",
                        timer_id
                    )));
                };
                if fired || *fired_at != scheduled_fire_at {
                    return Err(WorkflowEngineError::HistoryConflict(format!(
                        "timer {} has an invalid fire record",
                        timer_id
                    )));
                }
                fired = true;
            }
            _ => {}
        }
    }

    Ok(TimerState {
        fire_at_millis,
        fired,
    })
}

fn append<R: WorkflowRuntime>(
    runtime: &mut R,
    workflow_id: &WorkflowId,
    expected_revision: u64,
    event: WorkflowEvent,
) -> Result<u64, WorkflowEngineError<R::Error>> {
    match runtime
        .append_event(workflow_id, expected_revision, event)
        .map_err(WorkflowEngineError::Runtime)?
    {
        AppendOutcome::Appended { new_revision } => Ok(new_revision),
        AppendOutcome::Conflict => Err(WorkflowEngineError::ConcurrencyConflict),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestError(&'static str);

    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    struct MockRuntime {
        now: u64,
        history: WorkflowHistory,
        armed: Vec<TimerArmRequest>,
    }

    impl MockRuntime {
        fn new() -> Self {
            Self {
                now: 1_000,
                history: WorkflowHistory::default(),
                armed: Vec::new(),
            }
        }
    }

    impl WorkflowRuntime for MockRuntime {
        type Error = TestError;

        fn now_millis(&mut self) -> u64 {
            self.now
        }

        fn load_history(
            &mut self,
            _workflow_id: &WorkflowId,
        ) -> Result<WorkflowHistory, Self::Error> {
            Ok(self.history.clone())
        }

        fn append_event(
            &mut self,
            _workflow_id: &WorkflowId,
            expected_revision: u64,
            event: WorkflowEvent,
        ) -> Result<AppendOutcome, Self::Error> {
            if expected_revision != self.history.revision {
                return Ok(AppendOutcome::Conflict);
            }
            self.history.events.push(event);
            self.history.revision += 1;
            Ok(AppendOutcome::Appended {
                new_revision: self.history.revision,
            })
        }

        fn dispatch_activity(
            &mut self,
            _request: crate::ActivityDispatchRequest,
        ) -> Result<crate::ActivityDispatchResult, Self::Error> {
            Err(TestError("activity dispatch not expected"))
        }
    }

    impl WorkflowTimerRuntime for MockRuntime {
        fn arm_timer(&mut self, request: TimerArmRequest) -> Result<(), Self::Error> {
            self.armed.push(request);
            Ok(())
        }
    }

    #[test]
    fn signal_delivery_is_durable_and_replayed_to_same_wait() {
        let workflow_id = WorkflowId::new("workflow:signal");
        let mut rt = MockRuntime::new();
        let signal_id = SignalId::new("event-1");

        assert_eq!(
            DurableSignalExecutor
                .notify(
                    &mut rt,
                    &workflow_id,
                    signal_id.clone(),
                    "approval",
                    b"yes".to_vec(),
                )
                .unwrap(),
            SignalNotifyOutcome::Recorded
        );
        assert_eq!(
            DurableSignalExecutor
                .receive(&mut rt, &workflow_id, "approval", 0)
                .unwrap(),
            SignalProgress::Received {
                signal_id: signal_id.clone(),
                payload: b"yes".to_vec(),
            }
        );
        let revision = rt.history.revision;

        assert_eq!(
            DurableSignalExecutor
                .receive(&mut rt, &workflow_id, "approval", 0)
                .unwrap(),
            SignalProgress::Received {
                signal_id,
                payload: b"yes".to_vec(),
            }
        );
        assert_eq!(rt.history.revision, revision);
    }

    #[test]
    fn signal_notification_is_idempotent_but_conflicts_on_changed_content() {
        let workflow_id = WorkflowId::new("workflow:notify");
        let mut rt = MockRuntime::new();
        let signal_id = SignalId::new("event-1");

        assert_eq!(
            DurableSignalExecutor
                .notify(
                    &mut rt,
                    &workflow_id,
                    signal_id.clone(),
                    "approval",
                    b"yes".to_vec(),
                )
                .unwrap(),
            SignalNotifyOutcome::Recorded
        );
        assert_eq!(
            DurableSignalExecutor
                .notify(
                    &mut rt,
                    &workflow_id,
                    signal_id.clone(),
                    "approval",
                    b"yes".to_vec(),
                )
                .unwrap(),
            SignalNotifyOutcome::AlreadyRecorded
        );
        assert!(matches!(
            DurableSignalExecutor.notify(
                &mut rt,
                &workflow_id,
                signal_id,
                "approval",
                b"no".to_vec(),
            ),
            Err(WorkflowEngineError::HistoryConflict(_))
        ));
    }

    #[test]
    fn signal_wait_occurrences_consume_distinct_signals_in_order() {
        let workflow_id = WorkflowId::new("workflow:signal-order");
        let mut rt = MockRuntime::new();
        for (id, payload) in [("event-1", b"a".to_vec()), ("event-2", b"b".to_vec())] {
            DurableSignalExecutor
                .notify(
                    &mut rt,
                    &workflow_id,
                    SignalId::new(id),
                    "message",
                    payload,
                )
                .unwrap();
        }

        assert!(matches!(
            DurableSignalExecutor
                .receive(&mut rt, &workflow_id, "message", 0)
                .unwrap(),
            SignalProgress::Received { payload, .. } if payload == b"a"
        ));
        assert!(matches!(
            DurableSignalExecutor
                .receive(&mut rt, &workflow_id, "message", 1)
                .unwrap(),
            SignalProgress::Received { payload, .. } if payload == b"b"
        ));
    }

    #[test]
    fn timer_schedule_is_rearmed_idempotently_until_deadline() {
        let workflow_id = WorkflowId::new("workflow:timer");
        let mut rt = MockRuntime::new();

        let first = DurableTimerExecutor
            .wait(&mut rt, &workflow_id, "retry", 0, 500)
            .unwrap();
        let second = DurableTimerExecutor
            .wait(&mut rt, &workflow_id, "retry", 0, 500)
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(rt.armed.len(), 2);
        assert_eq!(rt.armed[0].timer_id, rt.armed[1].timer_id);
        assert_eq!(rt.history.events.len(), 1);
    }

    #[test]
    fn timer_fires_from_persisted_deadline_after_recovery() {
        let workflow_id = WorkflowId::new("workflow:timer-recovery");
        let mut rt = MockRuntime::new();

        assert!(matches!(
            DurableTimerExecutor
                .wait(&mut rt, &workflow_id, "wake", 0, 500)
                .unwrap(),
            TimerProgress::Waiting { .. }
        ));
        rt.now = 1_500;
        assert!(matches!(
            DurableTimerExecutor
                .wait(&mut rt, &workflow_id, "wake", 0, 500)
                .unwrap(),
            TimerProgress::Fired {
                fire_at_millis: 1_500,
                ..
            }
        ));

        let revision = rt.history.revision;
        rt.now = 9_999;
        assert!(matches!(
            DurableTimerExecutor
                .wait(&mut rt, &workflow_id, "wake", 0, 500)
                .unwrap(),
            TimerProgress::Fired {
                fire_at_millis: 1_500,
                ..
            }
        ));
        assert_eq!(rt.history.revision, revision);
    }

    #[test]
    fn timer_delay_change_fails_closed() {
        let workflow_id = WorkflowId::new("workflow:timer-change");
        let mut rt = MockRuntime::new();
        DurableTimerExecutor
            .wait(&mut rt, &workflow_id, "wake", 0, 500)
            .unwrap();

        assert!(matches!(
            DurableTimerExecutor.wait(&mut rt, &workflow_id, "wake", 0, 600),
            Err(WorkflowEngineError::HistoryConflict(_))
        ));
    }
}
