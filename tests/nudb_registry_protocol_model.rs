//! Bounded abstract model of managed WAL generation publication.
//! Run independently with rustc --edition=2021 --test; no new dependencies.

#[test]
fn ordered_publication_preserves_recoverability_at_every_crash_boundary() {
    let visited = explore(Policy::Ordered).expect("ordered protocol must recover");
    // Every legal durable combination must actually be explored, including
    // both outcomes of a crash while a replacement is pending.
    for (registry, wal) in [
        (Registry::Old, Binding::Old),
        (Registry::Pending, Binding::Old),
        (Registry::Pending, Binding::New),
        (Registry::New, Binding::New),
    ] {
        assert!(
            visited.contains(&State {
                registry,
                durable_registry: registry,
                wal,
                durable_wal: wal,
                phase: Phase::Recover,
            }),
            "missing recovery case: {registry:?}, {wal:?}"
        );
    }
    for phase in [
        Phase::Ready,
        Phase::ReserveRenamed,
        Phase::Reserved,
        Phase::WalRenamed,
        Phase::WalSynced,
        Phase::ReconcileRenamed,
        Phase::Recover,
    ] {
        assert!(
            visited.iter().any(|s| s.phase == phase),
            "missing {phase:?}"
        );
    }
    assert!(visited.contains(&State {
        registry: Registry::New,
        durable_registry: Registry::New,
        wal: Binding::New,
        durable_wal: Binding::New,
        phase: Phase::Ready,
    }));
    println!("checked {} reachable abstract states", visited.len());
}

#[test]
fn model_detects_missing_durable_reservation() {
    let trace = explore(Policy::SkipReservationSync).unwrap_err();
    assert!(trace.contains("unprotected WAL"), "{trace}");
}

#[test]
fn model_detects_reconciliation_before_wal_directory_sync() {
    let trace = explore(Policy::EarlyReconcile).unwrap_err();
    assert!(trace.contains("unprotected WAL"), "{trace}");
}

use std::collections::{HashSet, VecDeque};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum Binding {
    Old,
    New,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum Registry {
    Old,
    Pending,
    New,
}

impl Registry {
    fn protects(self, wal: Binding) -> bool {
        matches!(
            (self, wal),
            (Self::Old, Binding::Old) | (Self::Pending, _) | (Self::New, Binding::New)
        )
    }

    fn reconciled(wal: Binding) -> Self {
        match wal {
            Binding::Old => Self::Old,
            Binding::New => Self::New,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
enum Phase {
    Ready,
    ReserveRenamed,
    Reserved,
    WalRenamed,
    WalSynced,
    ReconcileRenamed,
    Recover,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
struct State {
    registry: Registry,
    durable_registry: Registry,
    wal: Binding,
    durable_wal: Binding,
    phase: Phase,
}

#[derive(Clone, Copy)]
enum Policy {
    Ordered,
    SkipReservationSync,
    EarlyReconcile,
}

impl State {
    fn initial() -> Self {
        Self {
            registry: Registry::Old,
            durable_registry: Registry::Old,
            wal: Binding::Old,
            durable_wal: Binding::Old,
            phase: Phase::Ready,
        }
    }

    fn successors(self, policy: Policy) -> Vec<(&'static str, Self)> {
        use Phase::*;
        let mut next = Vec::new();

        // Both temporary file contents are assumed synced before rename.
        // A power interruption may preserve or lose each unsynced rename.
        // Once its parent directory is synced, the old name is impossible.
        for registry in [self.durable_registry, self.registry] {
            for wal in [self.durable_wal, self.wal] {
                next.push((
                    "power interruption",
                    Self {
                        registry,
                        durable_registry: registry,
                        wal,
                        durable_wal: wal,
                        phase: Recover,
                    },
                ));
            }
        }

        let transition = match self.phase {
            Ready if self.registry == Registry::Old => Some((
                "rename pending registry",
                Self {
                    registry: Registry::Pending,
                    phase: ReserveRenamed,
                    ..self
                },
            )),
            Ready => None,
            ReserveRenamed if matches!(policy, Policy::SkipReservationSync) => Some((
                "BUG: skip reservation directory sync",
                Self {
                    phase: Reserved,
                    ..self
                },
            )),
            ReserveRenamed => Some((
                "sync reservation directory",
                Self {
                    durable_registry: self.registry,
                    phase: Reserved,
                    ..self
                },
            )),
            Reserved => Some((
                "rename replacement WAL",
                Self {
                    wal: Binding::New,
                    phase: WalRenamed,
                    ..self
                },
            )),
            WalRenamed if matches!(policy, Policy::EarlyReconcile) => Some((
                "BUG: reconcile without WAL directory sync",
                Self {
                    registry: Registry::New,
                    phase: ReconcileRenamed,
                    ..self
                },
            )),
            WalRenamed => Some((
                "sync replacement WAL directory",
                Self {
                    durable_wal: self.wal,
                    phase: WalSynced,
                    ..self
                },
            )),
            WalSynced => Some((
                "rename reconciled registry",
                Self {
                    registry: Registry::reconciled(self.wal),
                    phase: ReconcileRenamed,
                    ..self
                },
            )),
            ReconcileRenamed => Some((
                "sync reconciliation directory",
                Self {
                    durable_registry: self.registry,
                    phase: Ready,
                    ..self
                },
            )),
            Recover if self.registry.protects(self.wal) => Some((
                "recover exact binding and sync WAL",
                Self {
                    durable_wal: self.wal,
                    phase: WalSynced,
                    ..self
                },
            )),
            Recover => None,
        };
        if let Some(transition) = transition {
            next.push(transition);
        }
        next
    }
}

/// Exhaust all reachable abstract states, including repeated crashes during
/// recovery. Breadth-first exploration returns the shortest violating trace.
/// This is a protocol model, not a filesystem emulator or proof of Rust code.
fn explore(policy: Policy) -> Result<HashSet<State>, String> {
    let mut queue = VecDeque::from([(State::initial(), Vec::<String>::new())]);
    let mut visited = HashSet::new();
    while let Some((state, trace)) = queue.pop_front() {
        if !visited.insert(state) {
            continue;
        }
        if state.phase == Phase::Recover && !state.registry.protects(state.wal) {
            return Err(format!("unprotected WAL: {state:?}\n{}", trace.join("\n")));
        }
        for (action, successor) in state.successors(policy) {
            if !visited.contains(&successor) {
                let mut next_trace = trace.clone();
                next_trace.push(format!("{action}: {successor:?}"));
                queue.push_back((successor, next_trace));
            }
        }
    }
    Ok(visited)
}
