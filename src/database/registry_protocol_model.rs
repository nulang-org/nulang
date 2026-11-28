//! Bounded abstract model of managed WAL generation publication.
//! Test-only; no new dependencies.

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

#[cfg(unix)]
#[test]
fn actual_managed_rotation_matches_model_through_repeated_recovery_interruptions() {
    use super::generation_registry::{GenerationRegistry, GenerationStatus};
    use super::interruption::{with_interruption_occurrence, StorageInterruptionPoint as Hook};
    use super::managed_storage::ManagedTablet;
    use super::sst_manifest::ManifestStore;
    use super::store::WalBackedTablet;
    use super::tablet::{KeyRange, TabletDescriptor, TabletId, TabletMutation, TransactionId};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn check_registry(
        registry: &GenerationRegistry,
        old: &ManifestStore,
        new: &ManifestStore,
        expected: Registry,
    ) {
        assert_eq!(
            registry.status(old).unwrap() == GenerationStatus::Referenced,
            expected.protects(Binding::Old),
        );
        assert_eq!(
            registry.status(new).unwrap() == GenerationStatus::Referenced,
            expected.protects(Binding::New),
        );
    }

    // Generate the uninterrupted forward path from the same transition
    // function used by the exhaustive explorer, not the storage engine.
    let mut path = vec![State::initial()];
    while let Some((_, next)) = path
        .last()
        .unwrap()
        .successors(Policy::Ordered)
        .into_iter()
        .find(|(label, _)| *label != "power interruption")
    {
        path.push(next);
    }
    let visited = explore(Policy::Ordered).unwrap();

    for (hook, occurrence, phase) in [
        (Hook::RegistryBeforeRename, 0, Phase::Ready),
        (Hook::RegistryAfterRename, 0, Phase::ReserveRenamed),
        (Hook::WalReclaimAfterReplacementWrite, 0, Phase::Reserved),
        (Hook::WalReclaimAfterReplacementSync, 0, Phase::Reserved),
        (Hook::WalReclaimAfterRename, 0, Phase::WalRenamed),
        (Hook::WalReclaimAfterDirectorySync, 0, Phase::WalSynced),
        (Hook::RegistryBeforeRename, 1, Phase::WalSynced),
        (Hook::RegistryAfterRename, 1, Phase::ReconcileRenamed),
    ] {
        let expected = *path.iter().find(|s| s.phase == phase).unwrap();
        let fixture = Fixture(std::env::temp_dir().join(format!(
            "nudb-protocol-replay-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )));
        fs::create_dir(&fixture.0).unwrap();
        let old_dir = fixture.0.join("old");
        let new_dir = fixture.0.join("new");
        let reg_dir = fixture.0.join("registry");
        for dir in [&old_dir, &new_dir, &reg_dir] {
            fs::create_dir(dir).unwrap();
        }
        let wal = fixture.0.join("tablet.wal");
        let descriptor = TabletDescriptor::new(
            TabletId::new(97).unwrap(),
            KeyRange::new(b"a".to_vec(), Some(b"z".to_vec())).unwrap(),
            7,
        )
        .unwrap();
        let mutation = |value: &[u8]| {
            vec![TabletMutation::Put {
                key: b"k".to_vec(),
                value: value.to_vec(),
            }]
        };
        let first = TransactionId::new([1; 16]);
        let second = TransactionId::new([2; 16]);
        let mut raw = WalBackedTablet::open(descriptor.clone(), &wal).unwrap();
        let first_receipt = raw.commit_idempotent(first, mutation(b"first")).unwrap();
        let mut old = ManifestStore::create(&old_dir, 97, 7).unwrap();
        raw.flush_sst_shadow(&mut old, 1).unwrap();
        raw.seal_sst_recovery(&mut old).unwrap();
        raw.reclaim_to_sealed_sst(&old).unwrap();
        let registry = GenerationRegistry::create(&reg_dir, 97, 7).unwrap();
        let mut managed = ManagedTablet::adopt(raw, registry, old).unwrap();
        let second_receipt = managed
            .commit_idempotent(second, mutation(b"second"))
            .unwrap();
        let next = ManifestStore::create(&new_dir, 97, 7).unwrap();
        let err =
            with_interruption_occurrence(hook, occurrence, || managed.rotate(next, 1)).unwrap_err();
        assert!(
            err.to_string()
                .contains("injected NulangDB storage interruption"),
            "{hook:?} occurrence {occurrence}: {err}"
        );
        assert!(managed.read_at(b"k", 2).is_err());
        drop(managed);
        let bytes = fs::read(&wal).unwrap();

        // An injected process-visible I/O error retains successful renames.
        // This is one legal outcome of the abstract power-loss model, not
        // a simulation of lost unsynced directory writes.
        let mut recovered = State {
            registry: expected.registry,
            durable_registry: expected.registry,
            wal: expected.wal,
            durable_wal: expected.wal,
            phase: Phase::Recover,
        };
        assert!(visited.contains(&recovered), "{recovered:?}");
        let (current, other) = match recovered.wal {
            Binding::Old => (&old_dir, &new_dir),
            Binding::New => (&new_dir, &old_dir),
        };
        let inspect = |state: State| {
            let registry = GenerationRegistry::open(&reg_dir, 97, 7).unwrap();
            let old = ManifestStore::open(&old_dir, 97, 7).unwrap();
            let new = ManifestStore::open(&new_dir, 97, 7).unwrap();
            check_registry(&registry, &old, &new, state.registry);
        };
        inspect(recovered);
        let registry = GenerationRegistry::open(&reg_dir, 97, 7).unwrap();
        assert!(ManagedTablet::open(descriptor.clone(), &wal, other, registry).is_err());
        assert_eq!(fs::read(&wal).unwrap(), bytes);

        // Interrupt reopening three more times, including before and after
        // the registry reconciliation rename. Compute each next expected
        // state by executing the model's recovery transitions.
        for recovery_hook in [
            Hook::RegistryBeforeWalSync,
            Hook::RegistryBeforeRename,
            Hook::RegistryAfterRename,
        ] {
            let registry = GenerationRegistry::open(&reg_dir, 97, 7).unwrap();
            let result = with_interruption_occurrence(recovery_hook, 0, || {
                ManagedTablet::open(descriptor.clone(), &wal, current, registry)
            });
            // An already reconciled registry is unchanged: the engine
            // deliberately omits the redundant publication and its hooks.
            if recovered.registry != Registry::Pending
                && recovery_hook != Hook::RegistryBeforeWalSync
            {
                drop(result.expect("already reconciled registry must reopen without rename"));
                inspect(recovered);
                assert_eq!(fs::read(&wal).unwrap(), bytes);
                continue;
            }
            let err = result.expect_err("expected recovery interruption");
            assert!(
                err.to_string()
                    .contains("injected NulangDB storage interruption"),
                "{err}"
            );
            if recovery_hook == Hook::RegistryAfterRename {
                let synced = recovered
                    .successors(Policy::Ordered)
                    .into_iter()
                    .find(|(_, state)| state.phase == Phase::WalSynced)
                    .unwrap()
                    .1;
                let renamed = synced
                    .successors(Policy::Ordered)
                    .into_iter()
                    .find(|(_, state)| state.phase == Phase::ReconcileRenamed)
                    .unwrap()
                    .1;
                recovered.registry = renamed.registry;
                recovered.durable_registry = renamed.registry;
            }
            assert!(visited.contains(&recovered));
            inspect(recovered);
            assert_eq!(fs::read(&wal).unwrap(), bytes);
        }
        let registry = GenerationRegistry::open(&reg_dir, 97, 7).unwrap();
        let mut managed = ManagedTablet::open(descriptor, &wal, current, registry).unwrap();
        assert_eq!(managed.current_sequence(), 2);
        assert_eq!(managed.read_at(b"k", 1).unwrap(), Some(b"first".as_slice()));
        assert_eq!(
            managed.read_at(b"k", 2).unwrap(),
            Some(b"second".as_slice())
        );
        assert_eq!(
            managed
                .commit_idempotent(first, mutation(b"first"))
                .unwrap(),
            first_receipt
        );
        assert_eq!(
            managed
                .commit_idempotent(second, mutation(b"second"))
                .unwrap(),
            second_receipt
        );
        let other = ManifestStore::open(other, 97, 7).unwrap();
        assert_ne!(
            managed.generation_status(&other).unwrap(),
            GenerationStatus::Referenced
        );
        assert_eq!(fs::read(&wal).unwrap(), bytes);
    }
}
