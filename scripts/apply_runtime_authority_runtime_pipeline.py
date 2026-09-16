from pathlib import Path


def replace_exact(path: str, old: str, new: str, expected: int = 1) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != expected:
        raise SystemExit(f"{path}: expected {expected} matches, found {count}")
    p.write_text(text.replace(old, new))


# ---------------------------------------------------------------------------
# VM: select grants by the same Spawn instruction PC used by init overrides,
# then pass the owned manifest through the callback boundary.
# ---------------------------------------------------------------------------
replace_exact(
    "src/vm.rs",
    '''    fn spawn_actor(
        &mut self,
        module: &CodeModule,
        behavior_idx: usize,
        init: Vec<(String, Value)>,
    ) -> Value;
''',
    '''    fn spawn_actor(
        &mut self,
        module: &CodeModule,
        behavior_idx: usize,
        init: Vec<(String, Value)>,
        capabilities: Vec<String>,
    ) -> Value;
''',
)

replace_exact(
    "src/vm.rs",
    '''    fn spawn_actor(
        &mut self,
        _module: &CodeModule,
        _behavior_idx: usize,
        _init: Vec<(String, Value)>,
    ) -> Value {
        Value::actor_ref(0)
    }
''',
    '''    fn spawn_actor(
        &mut self,
        _module: &CodeModule,
        _behavior_idx: usize,
        _init: Vec<(String, Value)>,
        _capabilities: Vec<String>,
    ) -> Value {
        Value::actor_ref(0)
    }
''',
)

replace_exact(
    "src/vm.rs",
    '''        let result = if let Some(module) = self.modules.get(module_idx) {
            self.actor_callbacks.spawn_actor(module, behavior_idx, init)
        } else {
            Value::actor_ref(0)
        };
''',
    '''        let capabilities = self
            .modules
            .get(module_idx)
            .and_then(|module| {
                module
                    .spawn_capability_grants
                    .iter()
                    .find(|(offset, _)| *offset == spawn_pc)
                    .map(|(_, grants)| grants.clone())
            })
            .unwrap_or_default();
        let result = if let Some(module) = self.modules.get(module_idx) {
            self.actor_callbacks
                .spawn_actor(module, behavior_idx, init, capabilities)
        } else {
            Value::actor_ref(0)
        };
''',
)

# ---------------------------------------------------------------------------
# Runtime callback bridges.
# ---------------------------------------------------------------------------
replace_exact(
    "src/runtime/callbacks.rs",
    '''    fn spawn_actor(
        &mut self,
        module: &crate::bytecode::CodeModule,
        behavior_idx: usize,
        init: Vec<(String, crate::vm::Value)>,
    ) -> crate::vm::Value {
''',
    '''    fn spawn_actor(
        &mut self,
        module: &crate::bytecode::CodeModule,
        behavior_idx: usize,
        init: Vec<(String, crate::vm::Value)>,
        capabilities: Vec<String>,
    ) -> crate::vm::Value {
''',
    expected=2,
)
replace_exact(
    "src/runtime/callbacks.rs",
    '''        self.runtime
            .borrow_mut()
            .spawn_from_module(module, behavior_idx, init)
''',
    '''        self.runtime
            .borrow_mut()
            .spawn_from_module_with_capabilities(module, behavior_idx, init, capabilities)
''',
)
replace_exact(
    "src/runtime/callbacks.rs",
    '''        unsafe { (*self.runtime).spawn_from_module(module, behavior_idx, init) }
''',
    '''        unsafe {
            (*self.runtime)
                .spawn_from_module_with_capabilities(module, behavior_idx, init, capabilities)
        }
''',
)

# ---------------------------------------------------------------------------
# AOT callback bridges. Actor-only callbacks cannot spawn through a Runtime;
# runtime-backed AOT callbacks use the same authority path as bytecode.
# ---------------------------------------------------------------------------
replace_exact(
    "src/aot/mod.rs",
    '''    fn spawn_actor(
        &mut self,
        _module: &crate::bytecode::CodeModule,
        _behavior_idx: usize,
        _init: Vec<(String, crate::vm::Value)>,
    ) -> crate::vm::Value {
        crate::vm::Value::actor_ref(0)
    }
''',
    '''    fn spawn_actor(
        &mut self,
        _module: &crate::bytecode::CodeModule,
        _behavior_idx: usize,
        _init: Vec<(String, crate::vm::Value)>,
        _capabilities: Vec<String>,
    ) -> crate::vm::Value {
        crate::vm::Value::actor_ref(0)
    }
''',
)
replace_exact(
    "src/aot/mod.rs",
    '''    fn spawn_actor(
        &mut self,
        module: &crate::bytecode::CodeModule,
        behavior_idx: usize,
        init: Vec<(String, crate::vm::Value)>,
    ) -> crate::vm::Value {
''',
    '''    fn spawn_actor(
        &mut self,
        module: &crate::bytecode::CodeModule,
        behavior_idx: usize,
        init: Vec<(String, crate::vm::Value)>,
        capabilities: Vec<String>,
    ) -> crate::vm::Value {
''',
    expected=2,
)
replace_exact(
    "src/aot/mod.rs",
    '''unsafe { (*self.runtime).spawn_from_module(module, behavior_idx, init) }''',
    '''unsafe {
            (*self.runtime)
                .spawn_from_module_with_capabilities(module, behavior_idx, init, capabilities)
        }''',
    expected=2,
)

# ---------------------------------------------------------------------------
# Runtime public compatibility wrapper and authority-aware entry point.
# ---------------------------------------------------------------------------
replace_exact(
    "src/runtime/mod.rs",
    '''    pub fn spawn_from_module(
        &mut self,
        module: &crate::bytecode::CodeModule,
        behavior_idx: usize,
        init: Vec<(String, Value)>,
    ) -> Value {
        spawn::spawn_from_module(self, module, behavior_idx, init)
    }
''',
    '''    pub fn spawn_from_module(
        &mut self,
        module: &crate::bytecode::CodeModule,
        behavior_idx: usize,
        init: Vec<(String, Value)>,
    ) -> Value {
        self.spawn_from_module_with_capabilities(module, behavior_idx, init, Vec::new())
    }

    /// Spawn from bytecode while installing an explicit runtime-authority
    /// manifest. Top-level/host code is the root grant point. When an actor
    /// is currently executing, the requested manifest is attenuated against
    /// that parent's authority, so actor code can only pass authority it
    /// already holds.
    pub fn spawn_from_module_with_capabilities(
        &mut self,
        module: &crate::bytecode::CodeModule,
        behavior_idx: usize,
        init: Vec<(String, Value)>,
        capabilities: Vec<String>,
    ) -> Value {
        spawn::spawn_from_module_with_capabilities(
            self,
            module,
            behavior_idx,
            init,
            capabilities,
        )
    }
''',
)

# Preserve authority in supervisor restart templates.
replace_exact(
    "src/runtime/mod.rs",
    '''            persistent: actor.persistent,
            is_workflow: actor.is_workflow,
            is_agent: actor.is_agent,
        });
''',
    '''            persistent: actor.persistent,
            is_workflow: actor.is_workflow,
            is_agent: actor.is_agent,
            capabilities: actor.capabilities.clone(),
        });
''',
)

# ---------------------------------------------------------------------------
# Runtime spawn construction. Authority is validated and attenuated before an
# Actor is inserted or enqueued, eliminating a runnable-without-authority gap.
# ---------------------------------------------------------------------------
replace_exact(
    "src/runtime/spawn.rs",
    "use std::collections::HashMap;\n",
    "use std::collections::{BTreeSet, HashMap};\n",
)

replace_exact(
    "src/runtime/spawn.rs",
    '''pub(crate) fn spawn_actor_with_models(
    rt: &mut Runtime,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
) -> u64 {
    spawn_actor_with_id(
        rt,
        fresh_actor_id(),
        init,
        state_models,
        persistent,
        workflow,
    )
}
''',
    '''pub(crate) fn spawn_actor_with_models(
    rt: &mut Runtime,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
) -> u64 {
    spawn_actor_with_models_and_capabilities(
        rt,
        init,
        state_models,
        persistent,
        workflow,
        BTreeSet::new(),
    )
}

pub(crate) fn spawn_actor_with_models_and_capabilities(
    rt: &mut Runtime,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
    capabilities: BTreeSet<String>,
) -> u64 {
    spawn_actor_with_id_and_capabilities(
        rt,
        fresh_actor_id(),
        init,
        state_models,
        persistent,
        workflow,
        capabilities,
    )
}
''',
)

replace_exact(
    "src/runtime/spawn.rs",
    '''pub(crate) fn spawn_actor_with_id(
    rt: &mut Runtime,
    id: u64,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
) -> u64 {
    let mut actor = Actor::new(id, format!("actor_{}", id), 0);
''',
    '''pub(crate) fn spawn_actor_with_id(
    rt: &mut Runtime,
    id: u64,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
) -> u64 {
    spawn_actor_with_id_and_capabilities(
        rt,
        id,
        init,
        state_models,
        persistent,
        workflow,
        BTreeSet::new(),
    )
}

pub(crate) fn spawn_actor_with_id_and_capabilities(
    rt: &mut Runtime,
    id: u64,
    init: Box<dyn FnOnce() -> Vec<(String, Value)>>,
    state_models: HashMap<String, StateModel>,
    persistent: bool,
    workflow: Option<&str>,
    capabilities: BTreeSet<String>,
) -> u64 {
    let mut actor = Actor::new(id, format!("actor_{}", id), 0);
    actor.capabilities = capabilities;
''',
)

replace_exact(
    "src/runtime/spawn.rs",
    '''pub(crate) fn spawn_from_module(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    behavior_idx: usize,
    init: Vec<(String, Value)>,
) -> Value {
    rt.register_module_grains(module);
''',
    '''pub(crate) fn spawn_from_module(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    behavior_idx: usize,
    init: Vec<(String, Value)>,
) -> Value {
    spawn_from_module_with_capabilities(rt, module, behavior_idx, init, Vec::new())
}

pub(crate) fn spawn_from_module_with_capabilities(
    rt: &mut Runtime,
    module: &crate::bytecode::CodeModule,
    behavior_idx: usize,
    init: Vec<(String, Value)>,
    capability_grants: Vec<String>,
) -> Value {
    let requested = match crate::runtime_authority::AuthoritySet::from_manifest(
        capability_grants.iter(),
    ) {
        Ok(authority) => authority,
        Err(error) => {
            tracing::warn!(%error, "refusing spawn with malformed runtime authority manifest");
            return Value::nil();
        }
    };

    let capability_manifest = if let Some(parent_id) = rt.current_actor {
        let Some(parent) = rt.actors.get(&parent_id) else {
            tracing::warn!(parent_id, "refusing actor-originated spawn from missing parent");
            return Value::nil();
        };
        let parent_authority = match crate::runtime_authority::AuthoritySet::from_manifest(
            &parent.capabilities,
        ) {
            Ok(authority) => authority,
            Err(error) => {
                tracing::warn!(parent_id, %error, "refusing spawn from malformed parent authority manifest");
                return Value::nil();
            }
        };
        match parent_authority.attenuate(requested.iter().cloned()) {
            Ok(authority) => authority.into_manifest(),
            Err(error) => {
                tracing::warn!(parent_id, %error, "refusing child authority escalation");
                return Value::nil();
            }
        }
    } else {
        requested.into_manifest()
    };

    rt.register_module_grains(module);
''',
)

replace_exact(
    "src/runtime/spawn.rs",
    '''        spawn_actor_with_models(
            rt,
            Box::new(move || {
''',
    '''        spawn_actor_with_models_and_capabilities(
            rt,
            Box::new(move || {
''',
)
replace_exact(
    "src/runtime/spawn.rs",
    '''            if matches!(role, ActorRole::Workflow) {
                Some(meta.name.as_str())
            } else {
                None
            },
        )
    } else {
        spawn_actor_with_models(rt, Box::new(move || init), HashMap::new(), false, None)
    };
''',
    '''            if matches!(role, ActorRole::Workflow) {
                Some(meta.name.as_str())
            } else {
                None
            },
            capability_manifest.clone(),
        )
    } else {
        spawn_actor_with_models_and_capabilities(
            rt,
            Box::new(move || init),
            HashMap::new(),
            false,
            None,
            capability_manifest.clone(),
        )
    };
''',
)

# ---------------------------------------------------------------------------
# Supervision: a restart must preserve the previous incarnation's authority,
# not silently erase or broaden it.
# ---------------------------------------------------------------------------
replace_exact(
    "src/runtime/supervisor.rs",
    '''    pub persistent: bool,
    pub is_workflow: bool,
    pub is_agent: bool,
}
''',
    '''    pub persistent: bool,
    pub is_workflow: bool,
    pub is_agent: bool,
    /// Exact runtime-authority manifest of the registered child incarnation.
    pub capabilities: std::collections::BTreeSet<String>,
}
''',
)
replace_exact(
    "src/runtime/supervisor.rs",
    '''        new_actor.persistent = template.persistent;
        new_actor.is_workflow = template.is_workflow;
        new_actor.is_agent = template.is_agent;
        new_actor.state = ActorState::Running;
''',
    '''        new_actor.persistent = template.persistent;
        new_actor.is_workflow = template.is_workflow;
        new_actor.is_agent = template.is_agent;
        new_actor.capabilities = template.capabilities.clone();
        new_actor.state = ActorState::Running;
''',
)

# ---------------------------------------------------------------------------
# Integration tests: malformed data fails closed, root grants install exactly,
# child grants attenuate, escalation is denied, and supervision snapshots the
# manifest for restart.
# ---------------------------------------------------------------------------
tests = Path("tests/runtime_authority_spawn_pipeline.rs")
text = tests.read_text()
text = text.replace(
    "use nulang::parser::Parser;\n",
    "use nulang::parser::Parser;\nuse nulang::runtime::{ChildSpec, RestartPolicy, RestartStrategy, Runtime};\n",
    1,
)
text += r'''

fn compiled_worker() -> (CodeModule, usize) {
    let source = format!("{}\nfn main() {{ 0 }}", ACTOR);
    let module = compile(&source).expect("compile worker module");
    let behavior_idx = module.actor_metadata[0].behavior_indices[0];
    (module, behavior_idx)
}

#[test]
fn host_root_grant_is_installed_exactly() {
    let (module, behavior_idx) = compiled_worker();
    let mut rt = Runtime::new();
    let actor_id = rt
        .spawn_from_module_with_capabilities(
            &module,
            behavior_idx,
            vec![],
            vec!["Net::TcpOut(api.example.com:443)".to_string()],
        )
        .as_actor_id()
        .expect("host-authorized spawn must succeed");
    let actor = rt.actors.get(&actor_id).expect("spawned actor exists");
    assert_eq!(
        actor.capabilities,
        std::collections::BTreeSet::from(["Net::TcpOut(api.example.com:443)".to_string()])
    );
}

#[test]
fn actor_child_can_only_attenuate_parent_authority() {
    let (module, behavior_idx) = compiled_worker();
    let mut rt = Runtime::new();
    let parent_id = rt
        .spawn_from_module_with_capabilities(
            &module,
            behavior_idx,
            vec![],
            vec![
                "Net::TcpOut(api.example.com:443)".to_string(),
                "Fs::Read(/data/models)".to_string(),
            ],
        )
        .as_actor_id()
        .expect("root spawn");

    rt.current_actor = Some(parent_id);
    let child_id = rt
        .spawn_from_module_with_capabilities(
            &module,
            behavior_idx,
            vec![],
            vec!["Net::TcpOut(api.example.com:443)".to_string()],
        )
        .as_actor_id()
        .expect("subset attenuation must succeed");
    assert_eq!(
        rt.actors.get(&child_id).unwrap().capabilities,
        std::collections::BTreeSet::from(["Net::TcpOut(api.example.com:443)".to_string()])
    );

    let before = rt.actor_count();
    let denied = rt.spawn_from_module_with_capabilities(
        &module,
        behavior_idx,
        vec![],
        vec!["Net::TcpOut(evil.example.com:443)".to_string()],
    );
    assert!(denied.as_actor_id().is_none());
    assert_eq!(rt.actor_count(), before, "denied spawn must create no actor");
}

#[test]
fn malformed_serialized_manifest_creates_no_actor() {
    let (module, behavior_idx) = compiled_worker();
    let mut rt = Runtime::new();
    let before = rt.actor_count();
    let denied = rt.spawn_from_module_with_capabilities(
        &module,
        behavior_idx,
        vec![],
        vec!["not-a-capability".to_string()],
    );
    assert!(denied.as_actor_id().is_none());
    assert_eq!(rt.actor_count(), before);
}

#[test]
fn supervisor_restart_template_preserves_capability_manifest() {
    let (module, behavior_idx) = compiled_worker();
    let mut rt = Runtime::new();
    let child_id = rt
        .spawn_from_module_with_capabilities(
            &module,
            behavior_idx,
            vec![],
            vec!["Net::TcpOut(api.example.com:443)".to_string()],
        )
        .as_actor_id()
        .expect("child spawn");
    let supervisor_id = rt.create_supervisor("authority-supervisor", RestartStrategy::OneForOne);
    rt.supervise_child(
        supervisor_id,
        ChildSpec::new("worker", RestartPolicy::Permanent),
        child_id,
    );
    let template = rt.supervisors[&supervisor_id].children[0]
        .0
        .restart
        .as_ref()
        .expect("restart template");
    assert_eq!(
        template.capabilities,
        std::collections::BTreeSet::from(["Net::TcpOut(api.example.com:443)".to_string()])
    );
}
'''
tests.write_text(text)

Path("RFC/0022-runtime-authority-spawn-grants.md").write_text(r'''# RFC 0022: Runtime Authority Spawn Grants

Status: Draft

## Summary

Nulang already carries spawn-site capability strings in its AST, HIR, MIR,
bytecode side tables, and `Actor::capabilities`. This RFC connects those
pieces into one fail-closed authority path without introducing a second token
format or changing the `ActorRef`/bytecode operand ABI.

A local source spawn may grant explicit canonical tokens:

```nula
spawn Worker() with [Net::TcpOut("api.example.com:443")]
```

The compiler canonicalizes this to `Net::TcpOut(api.example.com:443)` and
records it against the exact `Spawn` instruction byte offset. The VM selects
that manifest by spawn PC and passes it through `ActorVmCallbacks`.

## Authority model

Top-level host execution is the root grant point. If no actor is currently
executing, a syntactically valid manifest may be installed on the child.

If an actor is executing, child authority is **attenuation only**. Every
requested token must already exist in the parent actor's exact validated
manifest. A child cannot mint a new host, port, filesystem path, or operation.

Malformed requested manifests, malformed parent manifests, missing parent
actors, and attempted escalation all fail closed: the runtime returns `nil`
and creates no child actor.

The existing CLI `--with=` resource gate remains a coarse compile/run policy
for `fs`, `net`, and `os` effect categories. It is not treated as proof that a
specific spawn token is delegated. Exact delegation is enforced at runtime.

## Lifecycle invariant

Authority is installed before an actor is inserted into `Runtime::actors` and
before it is enqueued. There is no interval in which a child can become
runnable with an empty or partially initialized manifest.

Supervised restart captures the live child's exact manifest in
`RestartTemplate` and restores it on the replacement actor. Restart therefore
preserves authority but never broadens it.

## Remote spawn

Remote spawn carrying authority is deliberately rejected by the compiler in
this phase. Secure remote delegation requires a versioned wire representation,
authenticated sender identity, replay protection, and an attenuation proof or
trusted-node policy. Silently dropping grants or transmitting unauthenticated
strings would create misleading security semantics.

## Deferred work

1. Enforce `Actor::capabilities` at every authority-bearing runtime host
   operation (HTTP/TCP, filesystem, process/FFI, providers) with operation-
   specific canonical resource normalization.
2. Version and authenticate remote capability propagation.
3. Define migration/hibernation persistence semantics for authority manifests.
4. Add revocation/expiry only if concrete use cases require dynamic authority;
   do not weaken exact-match default-deny semantics with wildcard shortcuts.
''')
