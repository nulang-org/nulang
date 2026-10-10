//! Deterministic Simulation Testing (DST) for Nulang.
//!
//! A `Simulator` replaces the real scheduler, network, and clock with
//! deterministic fakes so that actor programs execute identically on every
//! run for a given seed. This enables reproducible debugging of concurrency
//! bugs, deadlock detection, and invariant checking.
//!
//! Usage:
//!   let mut sim = Simulator::new(42); // seed = 42
//!   sim.load_program("examples/pingpong.nula");
//!   sim.run_until_quiescence();
//!   assert!(!sim.has_deadlock());

use std::collections::{HashMap, VecDeque};

/// Seed count for DST seed-sweep tests. Defaults to `default`; the
/// `NULANG_DST_SEEDS` environment variable overrides it so CI can scale a
/// sweep to 10⁴ seeds without editing the test (see
/// `.github/workflows/dst-nightly.yml`). Same seed → same run, so a
/// higher count is purely more interleavings covered.
pub fn dst_seed_count(default: u64) -> u64 {
    std::env::var("NULANG_DST_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// A deterministic pseudo-random number generator (splitmix64).
#[derive(Debug, Clone)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    pub fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    /// Pick a random element from a slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            let idx = (self.next() as usize) % items.len();
            Some(&items[idx])
        }
    }
}

/// `rand_core::RngCore` view of the deterministic splitmix64 generator, so
/// the same seeded sequence can drive every random decision in the runtime
/// (scheduler selection, cluster gossip/repair picks) — a same-seed run is
/// then bit-reproducible end to end.
impl rand_core::RngCore for DeterministicRng {
    fn next_u32(&mut self) -> u32 {
        self.next() as u32
    }

    fn next_u64(&mut self) -> u64 {
        self.next()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            let bytes = self.next().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

/// A simulated actor in the DST framework.
#[derive(Debug, Clone)]
pub struct SimActor {
    pub id: u64,
    pub name: String,
    pub mailbox: VecDeque<SimMessage>,
    pub state: HashMap<String, SimValue>,
}

/// A message in the simulated system.
#[derive(Debug, Clone)]
pub struct SimMessage {
    pub sender: u64,
    pub target: u64,
    pub behavior: String,
    pub payload: Vec<SimValue>,
}

/// Simplified value type for DST (subset of Nulang values).
#[derive(Debug, Clone, PartialEq)]
pub enum SimValue {
    Int(i64),
    Float(f64),
    Bool(bool),
    String(String),
    Nil,
    Unit,
}

/// Result of one simulation step.
#[derive(Debug, Clone)]
pub enum StepResult {
    /// An actor processed a message.
    MessageProcessed { actor: u64, behavior: String },
    /// The configured simulation step budget has been exhausted.
    NoProgress,
    /// All mailboxes are empty and no timers are pending.
    Quiescent,
}

/// The deterministic simulator.
pub struct Simulator {
    pub rng: DeterministicRng,
    pub actors: HashMap<u64, SimActor>,
    pub pending_messages: VecDeque<SimMessage>,
    pub step_count: u64,
    pub max_steps: u64,
    pub clock_ms: u64,
    pub timers: Vec<(u64, u64, SimMessage)>, // (fire_at_ms, actor_id, message)
}

impl Simulator {
    /// Create a new simulator with the given random seed.
    pub fn new(seed: u64) -> Self {
        Self {
            rng: DeterministicRng::new(seed),
            actors: HashMap::new(),
            pending_messages: VecDeque::new(),
            step_count: 0,
            max_steps: 1_000_000,
            clock_ms: 0,
            timers: Vec::new(),
        }
    }

    /// Set the maximum number of steps before the simulation aborts.
    pub fn with_max_steps(mut self, max: u64) -> Self {
        self.max_steps = max;
        self
    }

    /// Register a simulated actor.
    pub fn register_actor(&mut self, id: u64, name: &str) {
        self.actors.insert(
            id,
            SimActor {
                id,
                name: name.to_string(),
                mailbox: VecDeque::new(),
                state: HashMap::new(),
            },
        );
    }

    /// Send a message to an actor.
    pub fn send(&mut self, sender: u64, target: u64, behavior: &str, payload: Vec<SimValue>) {
        self.pending_messages.push_back(SimMessage {
            sender,
            target,
            behavior: behavior.to_string(),
            payload,
        });
    }

    /// Advance the simulation by one step.
    pub fn step(&mut self) -> StepResult {
        if self.step_count >= self.max_steps {
            return StepResult::NoProgress;
        }
        self.step_count += 1;

        // Deliver any pending messages
        while let Some(msg) = self.pending_messages.pop_front() {
            if let Some(actor) = self.actors.get_mut(&msg.target) {
                actor.mailbox.push_back(msg);
            }
        }

        // Timer callbacks become runnable at the current virtual time. When
        // all actors are idle, jump to the *earliest* deadline and try again
        // within this step, so run_until_quiescence cannot stop prematurely.
        // Timers targeting stopped/missing actors are consumed as well.
        loop {
            let now = self.clock_ms;
            let mut fired = Vec::new();
            self.timers.retain(|(fire_at, actor_id, msg)| {
                if *fire_at <= now {
                    fired.push((*actor_id, msg.clone()));
                    false
                } else {
                    true
                }
            });
            for (actor_id, msg) in fired {
                if let Some(actor) = self.actors.get_mut(&actor_id) {
                    actor.mailbox.push_back(msg);
                }
            }

            // A HashMap's randomized iteration order cannot define the seeded
            // scheduler's choice set. Sort IDs before consuming the RNG.
            let mut ready: Vec<u64> = self
                .actors
                .iter()
                .filter(|(_, actor)| !actor.mailbox.is_empty())
                .map(|(id, _)| *id)
                .collect();
            ready.sort_unstable();

            if let Some(&actor_id) = self.rng.pick(&ready) {
                let actor = self
                    .actors
                    .get_mut(&actor_id)
                    .expect("ready actor must still be registered");
                let msg = actor
                    .mailbox
                    .pop_front()
                    .expect("ready actor must have a message");
                // This lightweight harness records delivery only; separate
                // real-runtime DST suites execute actual Nulang behaviors.
                return StepResult::MessageProcessed {
                    actor: actor_id,
                    behavior: msg.behavior,
                };
            }

            match self.timers.iter().map(|(fire_at, _, _)| *fire_at).min() {
                Some(next_deadline) => {
                    self.clock_ms = self.clock_ms.max(next_deadline);
                }
                None => return StepResult::Quiescent,
            }
        }
    }

    /// Run the simulation until quiescence or step limit.
    pub fn run_until_quiescence(&mut self) {
        loop {
            match self.step() {
                StepResult::Quiescent | StepResult::NoProgress => break,
                _ => {}
            }
        }
    }

    /// Check if any actors have non-empty mailboxes (potential deadlock).
    pub fn has_deadlock(&self) -> bool {
        self.actors.values().any(|a| !a.mailbox.is_empty())
    }

    /// Returns the number of steps executed.
    pub fn step_count(&self) -> u64 {
        self.step_count
    }

    /// Returns the current simulated clock in milliseconds.
    pub fn clock_ms(&self) -> u64 {
        self.clock_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simulator_pingpong() {
        let mut sim = Simulator::new(42);
        sim.register_actor(1, "pinger");
        sim.register_actor(2, "ponger");
        sim.send(0, 1, "ping", vec![]);
        sim.run_until_quiescence();
        assert!(!sim.has_deadlock());
        assert!(sim.step_count() > 0);
    }

    #[test]
    fn test_deterministic_rng() {
        let mut rng1 = DeterministicRng::new(123);
        let mut rng2 = DeterministicRng::new(123);
        for _ in 0..100 {
            assert_eq!(rng1.next(), rng2.next());
        }
    }

    #[test]
    fn test_different_seeds_diverge() {
        let mut rng1 = DeterministicRng::new(1);
        let mut rng2 = DeterministicRng::new(2);
        let v1: Vec<u64> = (0..10).map(|_| rng1.next()).collect();
        let v2: Vec<u64> = (0..10).map(|_| rng2.next()).collect();
        assert_ne!(v1, v2);
    }

    fn actor_trace(seed: u64, registration_order: &[u64]) -> Vec<u64> {
        let mut sim = Simulator::new(seed);
        for &id in registration_order {
            sim.register_actor(id, "worker");
        }
        for &id in registration_order {
            for _ in 0..3 {
                sim.send(0, id, "work", vec![]);
            }
        }

        let mut trace = Vec::new();
        loop {
            match sim.step() {
                StepResult::MessageProcessed { actor, .. } => trace.push(actor),
                StepResult::Quiescent => return trace,
                StepResult::NoProgress => panic!("simulation exhausted its step budget"),
            }
        }
    }

    #[test]
    fn test_dst_same_seed_same_actor_trace_regardless_of_registration_order() {
        let ascending: Vec<u64> = (1..=8).collect();
        let descending: Vec<u64> = ascending.iter().rev().copied().collect();

        // Separate HashMaps receive independent randomized hash states. Their
        // bucket iteration order must not influence seeded scheduling.
        for seed in 0..32 {
            let expected = actor_trace(seed, &ascending);
            assert_eq!(expected.len(), 24);
            assert_eq!(
                expected,
                actor_trace(seed, &descending),
                "seed {seed} yielded an insertion/hash-order-dependent trace"
            );
        }
    }

    #[test]
    fn test_dst_run_until_quiescence_fires_future_timer() {
        let mut sim = Simulator::new(42);
        sim.register_actor(1, "timer-worker");
        sim.timers.push((
            25,
            1,
            SimMessage {
                sender: 0,
                target: 1,
                behavior: "wake".to_string(),
                payload: vec![],
            },
        ));

        sim.run_until_quiescence();

        assert_eq!(sim.clock_ms(), 25);
        assert!(sim.timers.is_empty(), "future timer was never fired");
        assert!(sim.actors[&1].mailbox.is_empty(), "fired timer was not processed");
        assert_eq!(sim.step_count(), 2, "one message step and one quiescence step");
    }

    #[test]
    fn test_dst_timer_uses_earliest_deadline_not_insertion_order() {
        let mut sim = Simulator::new(42);
        sim.register_actor(1, "timer-worker");
        for (deadline, name) in [(100, "late"), (20, "early")] {
            sim.timers.push((
                deadline,
                1,
                SimMessage {
                    sender: 0,
                    target: 1,
                    behavior: name.to_string(),
                    payload: vec![],
                },
            ));
        }

        assert!(matches!(
            sim.step(),
            StepResult::MessageProcessed { actor: 1, ref behavior } if behavior == "early"
        ));
        assert_eq!(sim.clock_ms(), 20);
        sim.run_until_quiescence();
        assert!(sim.timers.is_empty());
        assert_eq!(sim.clock_ms(), 100);
    }

    #[test]
    fn test_quiescence_detection() {
        let mut sim = Simulator::new(0).with_max_steps(100);
        sim.register_actor(1, "worker");
        sim.send(0, 1, "work", vec![SimValue::Int(42)]);
        sim.run_until_quiescence();
        assert_eq!(sim.has_deadlock(), false);
    }
}
