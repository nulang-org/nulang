//! Supervision trees: actor failure recovery.
//!
//! Implements Erlang/OTP-style supervision with:
//! - Four restart strategies: OneForOne, OneForAll, RestForOne,
//!   SimpleOneForOne (dynamic children spawned on demand from one template)
//! - Three restart policies: Permanent, Temporary, Transient
//! - Rate-limited restarts with configurable time windows
//! - Hierarchical supervision with escalation

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::*;
use crate::types::ExitReason;

use tracing::warn;

// ---------------------------------------------------------------------------
// Supervisor Action
// ---------------------------------------------------------------------------

/// Action taken by a supervisor after handling a child exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorAction {
    /// Child was restarted: contains the new actor_id.
    Restarted(u64),
    /// Max restarts exceeded: shut down the supervisor.
    Shutdown,
    /// No action taken (temporary child, normal exit, etc.).
    Ignore,
    /// Propagate failure to parent supervisor.
    Escalate,
}

// ---------------------------------------------------------------------------
// Restart Strategy & Policy
// ---------------------------------------------------------------------------

/// How a supervisor handles child actor failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
    /// Restart only the failed child.
    OneForOne,
    /// Restart all children when one fails.
    OneForAll,
    /// Restart the failed child and all children started after it.
    RestForOne,
    /// Dynamic children: the supervisor holds a single child template and
    /// children are spawned on demand via `Supervisor::start_child`. A
    /// failed child is replaced by a fresh instance of the template.
    SimpleOneForOne,
}

/// When to restart a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Always restart (even on normal exit).
    Permanent,
    /// Never restart.
    Temporary,
    /// Restart only on abnormal (non-normal) exit.
    Transient,
    /// Re-spawn the child on another node when its home node is confirmed
    /// gone (RFC 0014). Implies `Permanent` semantics for the local exit
    /// protocol, plus shadow replication + directory registration.
    RespawnOnNodeLoss,
}

// ---------------------------------------------------------------------------
// Child Specification
// ---------------------------------------------------------------------------

/// Specification for a supervised child actor.
#[derive(Debug, Clone)]
pub struct ChildSpec {
    /// Unique identifier for this child within the supervisor.
    pub id: String,
    /// Restart policy for this child.
    pub restart_policy: RestartPolicy,
    /// Maximum number of restarts allowed within the time window.
    pub max_restarts: u32,
    /// Time window (in seconds) for counting restarts.
    pub restart_window_secs: u32,
    /// Everything needed to rebuild the child on restart, captured from the
    /// live actor at registration time (`Runtime::supervise_child`).
    /// `None` means the child cannot be rebuilt faithfully; restart attempts
    /// then fail loudly (log + escalate) instead of creating a zombie actor
    /// that silently drops every message it receives.
    pub restart: Option<RestartTemplate>,
}

impl ChildSpec {
    /// Create a new child spec with the given ID and restart policy.
    ///
    /// Defaults: max_restarts=5, restart_window_secs=60.
    pub fn new(id: impl Into<String>, policy: RestartPolicy) -> Self {
        ChildSpec {
            id: id.into(),
            restart_policy: policy,
            max_restarts: 5,
            restart_window_secs: 60,
            restart: None,
        }
    }

    /// Set custom restart intensity limits.
    pub fn with_limits(mut self, max_restarts: u32, window_secs: u32) -> Self {
        self.max_restarts = max_restarts;
        self.restart_window_secs = window_secs;
        self
    }
}

// ---------------------------------------------------------------------------
// Restart Template
// ---------------------------------------------------------------------------

/// Snapshot of a child actor taken at registration time, used to rebuild it
/// on restart.  Mirrors what `Runtime::spawn_actor` and bytecode behavior
/// registration wire into a fresh actor: state fields, the native behavior
/// table, and the bytecode module with its behavior offsets.
///
/// State values are captured shallowly: heap-pointer values still refer to
/// the old incarnation's heap and are only meaningful while that heap is
/// alive or retired.
#[derive(Debug, Clone)]
pub struct RestartTemplate {
    /// Initial state fields captured at registration.
    pub state_data: Vec<(String, Value)>,
    /// Persistence model per state field.
    pub state_models: HashMap<String, StateModel>,
    /// Native behavior handlers (name, handler).
    pub behaviors: Vec<(String, fn(&mut Actor, &[Value]))>,
    /// Bytecode module backing the child's bytecode behaviors, if any.
    pub bytecode_module: Option<crate::bytecode::CodeModule>,
    /// Bytecode behavior offsets by behavior id.
    pub bytecode_offsets: Vec<usize>,
    /// Saga compensation offsets by behavior id.
    pub compensation_offsets: Vec<Option<usize>>,
    pub persistent: bool,
    pub is_workflow: bool,
    pub is_agent: bool,
}

// ---------------------------------------------------------------------------
// Child Template (simple_one_for_one dynamic children)
// ---------------------------------------------------------------------------

/// Template for the dynamic children of a `SimpleOneForOne` supervisor.
///
/// Carries everything `Runtime::spawn_from_module` needs to spawn a real
/// bytecode actor of the template type: the module that declares the actor
/// type plus a behavior-table index belonging to it. The actor type's
/// `ActorMeta` (state defaults, persistence models) is read from the module
/// on every spawn, so fresh children always start from the declared
/// defaults. Captured by `Runtime::perform_otp_builtin` on
/// `perform Otp.set_template(sup, "ActorTypeName")`.
#[derive(Debug, Clone)]
pub struct ChildTemplate {
    /// Actor type name the template was resolved from (for diagnostics and
    /// child-spec naming).
    pub type_name: String,
    /// Bytecode module declaring the actor type.
    pub module: crate::bytecode::CodeModule,
    /// A behavior-table index belonging to the actor type, as expected by
    /// `Runtime::spawn_from_module`.
    pub behavior_idx: usize,
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// A supervisor node in the supervision tree.
///
/// Each supervisor is itself an actor (identified by `id`) and manages a set
/// of child actors according to a restart strategy. If a child fails, the
/// supervisor applies the strategy to determine which children to restart.
///
/// If the maximum restart intensity is exceeded for any child, the supervisor
/// itself shuts down (returning `SupervisorAction::Shutdown`), which typically
/// causes escalation to a parent supervisor.
pub struct Supervisor {
    /// The actor ID of this supervisor.
    pub id: u64,
    /// Human-readable name.
    pub name: String,
    /// Restart strategy applied when a child fails.
    pub strategy: RestartStrategy,
    /// Children: (spec, actor_id) pairs in start order.
    pub children: Vec<(ChildSpec, u64)>,
    /// Restart history: (spec_id, restart_time) for rate limiting.
    /// Uses spec.id (String) so that restarts are tracked per child spec,
    /// not per actor_id (which changes on each restart).
    pub restart_history: Vec<(String, Instant)>,
    /// Parent supervisor actor ID (if any).
    pub parent: Option<u64>,
    /// Child template for `SimpleOneForOne` supervisors: the one actor type
    /// dynamic children are spawned from. Unused by the other strategies.
    pub template: Option<ChildTemplate>,
    /// Monotonic sequence numbering dynamically started children, so each
    /// gets a distinct child-spec id (restart rate limits are per spec id).
    next_dynamic_id: u64,
}

impl Supervisor {
    /// Create a new supervisor.
    ///
    /// The `id` should be the actor ID of the supervisor actor itself.
    pub fn new(id: u64, name: impl Into<String>, strategy: RestartStrategy) -> Self {
        Supervisor {
            id,
            name: name.into(),
            strategy,
            children: Vec::new(),
            restart_history: Vec::new(),
            parent: None,
            template: None,
            next_dynamic_id: 0,
        }
    }

    /// Register a child actor under this supervisor.
    pub fn add_child(&mut self, spec: ChildSpec, actor_id: u64) {
        self.children.push((spec, actor_id));
    }

    /// Find the child spec for a given actor ID.
    fn find_child_spec(&self, actor_id: u64) -> Option<&ChildSpec> {
        self.children
            .iter()
            .find(|(_, id)| *id == actor_id)
            .map(|(spec, _)| spec)
    }

    /// Find the index of a child by actor ID.
    fn find_child_index(&self, actor_id: u64) -> Option<usize> {
        self.children.iter().position(|(_, id)| *id == actor_id)
    }

    /// Update a child's actor ID (used after restart).
    fn update_child_id(&mut self, old_id: u64, new_id: u64) {
        if let Some((_, id)) = self.children.iter_mut().find(|(_, id)| *id == old_id) {
            *id = new_id;
        }
    }

    /// Remove a child by actor ID.
    fn remove_child(&mut self, actor_id: u64) {
        self.children.retain(|(_, id)| *id != actor_id);
    }

    /// Prune old restart history entries that fall outside all children's
    /// time windows. This prevents unbounded growth.
    fn prune_restart_history(&mut self) {
        let now = Instant::now();
        let max_window_secs = self
            .children
            .iter()
            .map(|(spec, _)| spec.restart_window_secs)
            .max()
            .unwrap_or(60);
        let max_window = Duration::from_secs(max_window_secs as u64);
        self.restart_history
            .retain(|(_, time)| now.duration_since(*time) < max_window);
    }

    /// Check if a child should be restarted based on its restart policy and
    /// the rate limit (max restarts within the time window).
    pub fn should_restart(
        &self,
        actor_id: u64,
        policy: RestartPolicy,
        reason: &ExitReason,
    ) -> bool {
        let policy_allows = match policy {
            RestartPolicy::Permanent => true,
            RestartPolicy::Temporary => false,
            RestartPolicy::Transient => !matches!(reason, ExitReason::Normal),
            RestartPolicy::RespawnOnNodeLoss => true,
        };
        if !policy_allows {
            return false;
        }
        let spec = match self.find_child_spec(actor_id) {
            Some(s) => s,
            None => return false,
        };
        let now = Instant::now();
        let window = Duration::from_secs(spec.restart_window_secs as u64);
        // Track restarts per spec.id (not actor_id, which changes on restart).
        let recent_restarts = self
            .restart_history
            .iter()
            .filter(|(spec_id, time)| *spec_id == spec.id && now.duration_since(*time) < window)
            .count() as u32;
        recent_restarts < spec.max_restarts
    }

    /// Rebuild a child actor from its restart template, mirroring
    /// `Runtime::spawn_actor` plus bytecode behavior registration.
    ///
    /// Returns the new actor id, or `None` if no template was captured — in
    /// that case the failure is logged so the caller can escalate instead of
    /// silently creating a zombie actor that drops every message.
    fn rebuild_child(
        &self,
        spec: &ChildSpec,
        runtime: &mut Runtime,
        old_actor_id: u64,
    ) -> Option<u64> {
        let template = match &spec.restart {
            Some(t) => t,
            None => {
                warn!(
                    "supervisor '{}': child '{}' has no restart template; \
                     refusing to restart it as a bare actor",
                    self.name, spec.id
                );
                return None;
            }
        };
        let new_id = fresh_actor_id();
        let child_name = format!("{}_child_{}", self.name, spec.id);
        let mut new_actor = Actor::new(new_id, child_name, 0);

        // Hydrate durable state from the persistence store if a snapshot
        // exists for the old actor.  On first spawn there is no snapshot,
        // so we fall back to the template state captured at registration.
        let snapshot = if template.persistent {
            runtime.persistence.load_snapshot(old_actor_id)
        } else {
            None
        };

        if let Some(ref snap) = snapshot {
            for (name, value) in &snap.state {
                let v = value.to_value_on_heap(&mut new_actor);
                new_actor.set_state_field(name.clone(), v);
            }
            for (name, value) in &template.state_data {
                if !snap.state.contains_key(name) {
                    new_actor.set_state_field(name.clone(), *value);
                }
            }
            new_actor.sequence = snap.sequence;
            new_actor.waiting_signal = snap.waiting_signal.clone();
        } else {
            for (name, value) in &template.state_data {
                new_actor.set_state_field(name.clone(), *value);
            }
        }

        new_actor.state_models = template.state_models.clone();
        for (name, handler) in &template.behaviors {
            new_actor.register_behavior(name.clone(), *handler);
        }
        new_actor.bytecode_offsets = template.bytecode_offsets.clone();
        new_actor.compensation_offsets = template.compensation_offsets.clone();
        new_actor.persistent = template.persistent;
        new_actor.is_workflow = template.is_workflow;
        new_actor.is_agent = template.is_agent;
        new_actor.state = ActorState::Running;
        new_actor.parent = Some(self.id);
        if let Some(module) = &template.bytecode_module {
            new_actor.bytecode_module = Some(module.clone());
            runtime.register_recovery_module(
                new_id,
                module.clone(),
                template.bytecode_offsets.clone(),
                template.compensation_offsets.clone(),
            );
        }
        let is_workflow = new_actor.is_workflow;
        runtime.actors.insert(new_id, new_actor);
        // Register CRDT-backed fields with the CrdtManager.
        if let Some(ref mut mgr) = runtime.crdt_manager {
            let actor = runtime.actors.get(&new_id).unwrap();
            mgr.register_actor_fields(new_id, actor);
        }
        if is_workflow {
            runtime.layout_workflow_behavior_table(new_id);
        }

        // Re-key the snapshot under the new actor id so future restarts find it.
        if let Some(mut snap) = snapshot {
            snap.actor_id = new_id;
            let _ = runtime.persistence.save_snapshot(snap);
            let _ = runtime.persistence.clear(old_actor_id);
        }

        // If the dead child was itself a supervisor, recreate its
        // `Supervisor` struct under the new actor id. Without this, a
        // supervised supervisor loses all supervision of its own children
        // after one restart: its old struct stays keyed by a dead actor id
        // and every `Otp.*` op on the new id silently no-ops.
        if runtime.supervisors.contains_key(&old_actor_id) {
            let old_sup = runtime
                .supervisors
                .remove(&old_actor_id)
                .expect("supervisor presence checked above");
            // Re-point surviving children's parent links at the new id so
            // their exits keep routing to a live supervisor.
            for (_, child_id) in &old_sup.children {
                if let Some(child) = runtime.actors.get_mut(child_id) {
                    if child.parent == Some(old_actor_id) {
                        child.parent = Some(new_id);
                    }
                }
            }
            let mut new_sup = Supervisor::new(new_id, old_sup.name.clone(), old_sup.strategy);
            new_sup.children = old_sup.children;
            new_sup.restart_history = old_sup.restart_history;
            new_sup.parent = old_sup.parent;
            new_sup.template = old_sup.template;
            new_sup.next_dynamic_id = old_sup.next_dynamic_id;
            runtime.supervisors.insert(new_id, new_sup);
        }

        runtime.scheduler.enqueue(new_id);
        Some(new_id)
    }

    /// Remove a child actor whose exit was already processed by
    /// `Runtime::handle_actor_exit` (its exit protocol — registry, process
    /// groups, monitor DOWN, link propagation — has run).  Only the reaping
    /// remains, retiring the heap while foreign references are outstanding.
    fn reap_exited_child(&self, actor_id: u64, runtime: &mut Runtime) {
        runtime.remove_actor_reaping(actor_id);
    }

    /// Restart a single child actor.
    pub fn restart_child(&mut self, actor_id: u64, runtime: &mut Runtime) -> Option<u64> {
        let now = Instant::now();
        let spec = self.find_child_spec(actor_id)?.clone();
        let window = Duration::from_secs(spec.restart_window_secs as u64);
        let recent_restarts = self
            .restart_history
            .iter()
            .filter(|(spec_id, time)| *spec_id == spec.id && now.duration_since(*time) < window)
            .count() as u32;
        if recent_restarts >= spec.max_restarts {
            return None;
        }
        self.reap_exited_child(actor_id, runtime);
        let new_id = match self.rebuild_child(&spec, runtime, actor_id) {
            Some(id) => id,
            None => {
                // rebuild_child logged the failure; drop the child so the
                // caller escalates instead of supervising a dead entry.
                self.remove_child(actor_id);
                return None;
            }
        };
        self.update_child_id(actor_id, new_id);
        self.restart_history.push((spec.id.clone(), now));
        self.prune_restart_history();
        Some(new_id)
    }

    /// Restart a failed dynamic child of a `SimpleOneForOne` supervisor by
    /// spawning a fresh instance of the child template: the new child starts
    /// from the actor type's declared state defaults (start-time init args
    /// are not replayed). Applies the same rate limiting as `restart_child`.
    fn restart_dynamic_child(&mut self, actor_id: u64, runtime: &mut Runtime) -> Option<u64> {
        let now = Instant::now();
        let spec = self.find_child_spec(actor_id)?.clone();
        let window = Duration::from_secs(spec.restart_window_secs as u64);
        let recent_restarts = self
            .restart_history
            .iter()
            .filter(|(spec_id, time)| *spec_id == spec.id && now.duration_since(*time) < window)
            .count() as u32;
        if recent_restarts >= spec.max_restarts {
            return None;
        }
        let template = match &self.template {
            Some(t) => t.clone(),
            None => {
                // No template: drop the child so the caller escalates
                // instead of supervising a dead entry (same posture as
                // rebuild_child's missing RestartTemplate).
                warn!(
                    "supervisor '{}': child '{}' cannot restart without a child template",
                    self.name, spec.id
                );
                self.remove_child(actor_id);
                return None;
            }
        };
        self.reap_exited_child(actor_id, runtime);
        let value = runtime.spawn_from_module(&template.module, template.behavior_idx, Vec::new());
        let new_id = match value.as_actor_id() {
            Some(id) => id,
            None => {
                self.remove_child(actor_id);
                return None;
            }
        };
        if let Some(actor) = runtime.actors.get_mut(&new_id) {
            actor.parent = Some(self.id);
        }
        self.update_child_id(actor_id, new_id);
        self.restart_history.push((spec.id.clone(), now));
        self.prune_restart_history();
        Some(new_id)
    }

    /// Restart all children (used by OneForAll strategy).
    ///
    /// `exited_id` is the child whose exit triggered the restart: its exit
    /// protocol already ran in `Runtime::handle_actor_exit`.  Every other
    /// child is still living, so its removal goes through the full exit
    /// protocol (registry unregister, process-group leave, monitor DOWN,
    /// link propagation) before reaping.
    pub fn restart_all(&mut self, runtime: &mut Runtime, exited_id: u64, reason: &ExitReason) {
        let now = Instant::now();
        let child_ids: Vec<u64> = self.children.iter().map(|(_, id)| *id).collect();
        for old_id in child_ids {
            let spec = match self.find_child_spec(old_id).cloned() {
                Some(s) => s,
                None => continue,
            };
            // Only the triggering child's rate limit was checked by the
            // caller (`handle_exit`). Each sibling must pass its own policy
            // and rate limit before being rebuilt, or a OneForAll group can
            // restart-loop forever even though every child's limits are
            // individually exhausted.
            if old_id != exited_id && !self.should_restart(old_id, spec.restart_policy, reason) {
                runtime.reap_living_actor(old_id, reason.clone());
                self.remove_child(old_id);
                continue;
            }
            if old_id == exited_id {
                self.reap_exited_child(old_id, runtime);
            } else {
                runtime.reap_living_actor(old_id, reason.clone());
            }
            match self.rebuild_child(&spec, runtime, old_id) {
                Some(new_id) => {
                    self.update_child_id(old_id, new_id);
                    self.restart_history.push((spec.id, now));
                }
                None => self.remove_child(old_id),
            }
        }
        self.prune_restart_history();
    }

    /// Restart the failed child and all children started after it
    /// (used by RestForOne strategy).
    ///
    /// Like `restart_all`, living siblings are removed through the full exit
    /// protocol; the triggering child's protocol already ran.
    pub fn restart_from(&mut self, actor_id: u64, runtime: &mut Runtime, reason: &ExitReason) {
        let idx = match self.find_child_index(actor_id) {
            Some(i) => i,
            None => return,
        };
        let now = Instant::now();
        let to_restart: Vec<(ChildSpec, u64)> = self
            .children
            .iter()
            .skip(idx)
            .map(|(spec, id)| (spec.clone(), *id))
            .collect();
        for (spec, old_id) in to_restart {
            // Same per-sibling rate-limit discipline as `restart_all`: a
            // sibling whose own policy forbids it or whose MaxR/MaxT is
            // exhausted is stopped and dropped, not rebuilt.
            if old_id != actor_id && !self.should_restart(old_id, spec.restart_policy, reason) {
                runtime.reap_living_actor(old_id, reason.clone());
                self.remove_child(old_id);
                continue;
            }
            if old_id == actor_id {
                self.reap_exited_child(old_id, runtime);
            } else {
                runtime.reap_living_actor(old_id, reason.clone());
            }
            match self.rebuild_child(&spec, runtime, old_id) {
                Some(new_id) => {
                    self.update_child_id(old_id, new_id);
                    self.restart_history.push((spec.id, now));
                }
                None => self.remove_child(old_id),
            }
        }
        self.prune_restart_history();
    }

    /// Spawn a fresh child from this supervisor's child template and
    /// supervise it (`SimpleOneForOne` only). Returns the new child's actor
    /// id, or `None` when the strategy is not `SimpleOneForOne` or no
    /// template has been set.
    ///
    /// The child is spawned through `Runtime::spawn_from_module`, so it is a
    /// real bytecode actor with the template type's state defaults and
    /// behavior table. Dynamic children default to the `Transient` restart
    /// policy: abnormal exits restart them from the template, while normal
    /// exits (and `terminate_child`) retire them without replacement.
    pub fn start_child(
        &mut self,
        runtime: &mut Runtime,
        init_args: Vec<(String, Value)>,
    ) -> Option<u64> {
        if self.strategy != RestartStrategy::SimpleOneForOne {
            return None;
        }
        let template = match &self.template {
            Some(t) => t.clone(),
            None => {
                warn!(
                    "supervisor '{}': start_child without a child template; \
                     set one via Otp.set_template first",
                    self.name
                );
                return None;
            }
        };
        let value = runtime.spawn_from_module(&template.module, template.behavior_idx, init_args);
        let child_id = value.as_actor_id()?;
        if let Some(actor) = runtime.actors.get_mut(&child_id) {
            actor.parent = Some(self.id);
        }
        let seq = self.next_dynamic_id;
        self.next_dynamic_id += 1;
        let spec = ChildSpec::new(
            format!("{}_{}", template.type_name, seq),
            RestartPolicy::Transient,
        );
        self.add_child(spec, child_id);
        Some(child_id)
    }

    /// Remove a child from supervision WITHOUT restarting it and exit it
    /// cleanly (`ExitReason::Normal`). Returns true when the child was
    /// supervised here. Contrasts with `Runtime::exit_actor`, which routes
    /// the exit through the child's restart policy and may replace it.
    pub fn terminate_child(&mut self, runtime: &mut Runtime, actor_id: u64) -> bool {
        if self.find_child_index(actor_id).is_none() {
            return false;
        }
        self.remove_child(actor_id);
        // Detach first so the Normal exit below never consults this
        // supervisor (no restart, no escalation).
        if let Some(actor) = runtime.actors.get_mut(&actor_id) {
            if actor.parent == Some(self.id) {
                actor.parent = None;
            }
        }
        runtime.exit_actor(actor_id, ExitReason::Normal);
        true
    }

    /// Handle a child exit according to the supervisor's restart strategy.
    pub fn handle_exit(
        &mut self,
        actor_id: u64,
        reason: ExitReason,
        runtime: &mut Runtime,
    ) -> SupervisorAction {
        let spec = match self.find_child_spec(actor_id) {
            Some(s) => s.clone(),
            None => return SupervisorAction::Ignore,
        };
        if !self.should_restart(actor_id, spec.restart_policy, &reason) {
            if spec.restart_policy == RestartPolicy::Temporary {
                self.remove_child(actor_id);
                runtime.remove_actor_reaping(actor_id);
                return SupervisorAction::Ignore;
            }
            if spec.restart_policy == RestartPolicy::Transient && reason == ExitReason::Normal {
                self.remove_child(actor_id);
                runtime.remove_actor_reaping(actor_id);
                return SupervisorAction::Ignore;
            }
            let now = Instant::now();
            let window = Duration::from_secs(spec.restart_window_secs as u64);
            let recent_restarts = self
                .restart_history
                .iter()
                .filter(|(spec_id, time)| *spec_id == spec.id && now.duration_since(*time) < window)
                .count() as u32;
            if recent_restarts >= spec.max_restarts {
                return SupervisorAction::Shutdown;
            }
            self.remove_child(actor_id);
            runtime.remove_actor_reaping(actor_id);
            return SupervisorAction::Ignore;
        }
        match self.strategy {
            RestartStrategy::OneForOne => match self.restart_child(actor_id, runtime) {
                Some(new_id) => SupervisorAction::Restarted(new_id),
                None => SupervisorAction::Shutdown,
            },
            RestartStrategy::OneForAll => {
                self.restart_all(runtime, actor_id, &reason);
                match self.children.iter().find(|(s, _)| s.id == spec.id) {
                    Some((_, new_id)) => SupervisorAction::Restarted(*new_id),
                    None => SupervisorAction::Escalate,
                }
            }
            RestartStrategy::RestForOne => {
                self.restart_from(actor_id, runtime, &reason);
                match self.children.iter().find(|(s, _)| s.id == spec.id) {
                    Some((_, new_id)) => SupervisorAction::Restarted(*new_id),
                    None => SupervisorAction::Escalate,
                }
            }
            RestartStrategy::SimpleOneForOne => {
                match self.restart_dynamic_child(actor_id, runtime) {
                    Some(new_id) => SupervisorAction::Restarted(new_id),
                    None => SupervisorAction::Shutdown,
                }
            }
        }
    }

    /// Count how many times a given child has been restarted within its time window.
    pub fn restart_count(&self, actor_id: u64) -> u32 {
        let now = Instant::now();
        let spec = match self.find_child_spec(actor_id) {
            Some(s) => s,
            None => return 0,
        };
        let window = Duration::from_secs(spec.restart_window_secs as u64);
        self.restart_history
            .iter()
            .filter(|(spec_id, time)| *spec_id == spec.id && now.duration_since(*time) < window)
            .count() as u32
    }

    /// Return the number of currently supervised children.
    pub fn child_count(&self) -> usize {
        self.children.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supervisor_action_debug() {
        let variants: Vec<SupervisorAction> = vec![
            SupervisorAction::Restarted(42),
            SupervisorAction::Shutdown,
            SupervisorAction::Ignore,
            SupervisorAction::Escalate,
        ];
        for v in &variants {
            let _ = format!("{:?}", v);
        }
    }

    #[test]
    fn test_restart_strategy_debug() {
        let variants = vec![
            RestartStrategy::OneForOne,
            RestartStrategy::OneForAll,
            RestartStrategy::RestForOne,
            RestartStrategy::SimpleOneForOne,
        ];
        for v in &variants {
            let _ = format!("{:?}", v);
        }
    }

    #[test]
    fn test_restart_policy_debug() {
        let variants = vec![
            RestartPolicy::Permanent,
            RestartPolicy::Temporary,
            RestartPolicy::Transient,
        ];
        for v in &variants {
            let _ = format!("{:?}", v);
        }
    }

    #[test]
    fn test_child_spec_new() {
        let spec = ChildSpec::new("test_child", RestartPolicy::Permanent);
        assert_eq!(spec.id, "test_child");
        assert_eq!(spec.restart_policy, RestartPolicy::Permanent);
        assert_eq!(spec.max_restarts, 5);
        assert_eq!(spec.restart_window_secs, 60);
    }

    #[test]
    fn test_supervisor_new() {
        let sup = Supervisor::new(1, "test_sup", RestartStrategy::OneForOne);
        assert_eq!(sup.id, 1);
        assert_eq!(sup.name, "test_sup");
        assert_eq!(sup.strategy, RestartStrategy::OneForOne);
        assert!(sup.children.is_empty());
        assert!(sup.parent.is_none());
    }

    #[test]
    fn test_supervisor_handle_exit_unknown() {
        let mut sup = Supervisor::new(1, "test_sup", RestartStrategy::OneForOne);
        let mut rt = Runtime::new();
        let action = sup.handle_exit(999, ExitReason::Error("unknown".into()), &mut rt);
        assert_eq!(action, SupervisorAction::Ignore);
    }
}
