use crate::types::{NuError, NuResult, Span};

#[cfg(feature = "wasm-backend")]
use wasmtime::component::*;
#[cfg(feature = "wasm-backend")]
use wasmtime::*;

#[cfg(feature = "wasm-backend")]
mod bindings {
    wasmtime::component::bindgen!({
        world: "actor",
        path: "wit/actor.wit",
    });
}

#[cfg(feature = "wasm-backend")]
use bindings::host::Host as HostTrait;
#[cfg(feature = "wasm-backend")]
use bindings::Actor;

#[cfg(feature = "wasm-backend")]
pub fn component_config() -> Config {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.consume_fuel(true);
    config.memory_reservation(4 << 30);
    config.memory_guard_size(128 << 20);
    config.cranelift_opt_level(OptLevel::Speed);
    config.wasm_simd(true);
    config
}

/// Resource policy applied independently to every component instance.
#[cfg(feature = "wasm-backend")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentLimits {
    /// Fuel replenished before every exported guest call.
    pub fuel_per_call: u64,
    /// Maximum linear-memory bytes available to one component instance.
    pub memory_bytes: usize,
    /// Maximum number of core instances created inside one store.
    pub max_store_instances: usize,
    /// Maximum number of linear memories created inside one store.
    pub max_memories: usize,
    /// Maximum number of tables created inside one store.
    pub max_tables: usize,
    /// Maximum number of idle instances retained by this runtime. The secure
    /// default is zero because a component without an explicit reset contract
    /// may retain guest state between calls.
    pub max_idle_instances: usize,
}

#[cfg(feature = "wasm-backend")]
impl Default for ComponentLimits {
    fn default() -> Self {
        Self {
            fuel_per_call: 10_000_000,
            memory_bytes: 64 << 20,
            max_store_instances: 64,
            max_memories: 8,
            max_tables: 16,
            max_idle_instances: 0,
        }
    }
}

/// Capability controls for the WASM component host.
///
/// Each flag determines whether a given host function is made available to
/// the guest component. If a flag is `false` and the component imports the
/// corresponding function, instantiation will fail at the linker level.
#[cfg(feature = "wasm-backend")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub allow_log: bool,
    pub allow_clock: bool,
    pub allow_random: bool,
}

#[cfg(feature = "wasm-backend")]
impl Default for Capabilities {
    fn default() -> Self {
        Capabilities {
            allow_log: false,
            allow_clock: false,
            allow_random: false,
        }
    }
}

#[cfg(feature = "wasm-backend")]
pub struct HostState {
    caps: Capabilities,
    log_messages: Vec<String>,
    limits: StoreLimits,
}

#[cfg(feature = "wasm-backend")]
impl HostTrait for HostState {
    fn log(&mut self, msg: String) {
        if self.caps.allow_log {
            self.log_messages.push(msg);
        }
    }

    fn clock_now(&mut self) -> u64 {
        if self.caps.allow_clock {
            0
        } else {
            panic!("clock capability denied")
        }
    }

    fn random_u64(&mut self) -> u64 {
        if self.caps.allow_random {
            0
        } else {
            panic!("random capability denied")
        }
    }
}

#[cfg(feature = "wasm-backend")]
struct PooledInstance {
    store: Store<HostState>,
    instance: wasmtime::component::Instance,
}

#[cfg(feature = "wasm-backend")]
pub struct ComponentRuntime {
    engine: Engine,
    component: Component,
    linker: wasmtime::component::Linker<HostState>,
    caps: Capabilities,
    limits: ComponentLimits,
    pool: std::sync::Mutex<Vec<PooledInstance>>,
}

#[cfg(feature = "wasm-backend")]
impl ComponentRuntime {
    pub fn new(wasm_bytes: &[u8]) -> NuResult<Self> {
        Self::new_with_caps(wasm_bytes, Capabilities::default())
    }

    pub fn new_with_caps(wasm_bytes: &[u8], caps: Capabilities) -> NuResult<Self> {
        Self::new_with_policy(wasm_bytes, caps, ComponentLimits::default())
    }

    pub fn new_with_policy(
        wasm_bytes: &[u8],
        caps: Capabilities,
        limits: ComponentLimits,
    ) -> NuResult<Self> {
        if limits.fuel_per_call == 0 {
            return Err(NuError::VMError {
                msg: "component fuel_per_call must be greater than zero".into(),
                span: Span::default(),
            });
        }
        if limits.memory_bytes == 0 {
            return Err(NuError::VMError {
                msg: "component memory_bytes must be greater than zero".into(),
                span: Span::default(),
            });
        }
        let config = component_config();
        let engine = Engine::new(&config).map_err(|e| NuError::VMError {
            msg: format!("wasmtime engine: {}", e),
            span: Span::default(),
        })?;
        let component = Component::new(&engine, wasm_bytes).map_err(|e| NuError::VMError {
            msg: format!("wasmtime component: {}", e),
            span: Span::default(),
        })?;
        let linker = Self::build_linker(&engine, caps)?;
        Ok(ComponentRuntime {
            engine,
            component,
            linker,
            caps,
            limits,
            pool: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn build_linker(
        engine: &Engine,
        caps: Capabilities,
    ) -> NuResult<wasmtime::component::Linker<HostState>> {
        let mut linker = wasmtime::component::Linker::<HostState>::new(engine);

        // Add the host instance manually with the correct name
        if caps.allow_log || caps.allow_clock || caps.allow_random {
            let mut inst =
                linker
                    .instance("nulang:runtime/host")
                    .map_err(|e| NuError::VMError {
                        msg: format!("wasmtime linker: {}", e),
                        span: Span::default(),
                    })?;
            if caps.allow_log {
                inst.func_wrap(
                    "log",
                    move |mut caller: wasmtime::StoreContextMut<'_, HostState>,
                          (msg,): (String,)| {
                        caller.data_mut().log_messages.push(msg);
                        Ok(())
                    },
                )
                .map_err(|e| NuError::VMError {
                    msg: format!("wasmtime linker: {}", e),
                    span: Span::default(),
                })?;
            }
            if caps.allow_clock {
                inst.func_wrap(
                    "clock-now",
                    move |mut caller: wasmtime::StoreContextMut<'_, HostState>, _: ()| {
                        let r = caller.data_mut().clock_now();
                        Ok((r,))
                    },
                )
                .map_err(|e| NuError::VMError {
                    msg: format!("wasmtime linker: {}", e),
                    span: Span::default(),
                })?;
            }
            if caps.allow_random {
                inst.func_wrap(
                    "random-u64",
                    move |mut caller: wasmtime::StoreContextMut<'_, HostState>, _: ()| {
                        let r = caller.data_mut().random_u64();
                        Ok((r,))
                    },
                )
                .map_err(|e| NuError::VMError {
                    msg: format!("wasmtime linker: {}", e),
                    span: Span::default(),
                })?;
            }
        }
        Ok(linker)
    }

    /// Acquire a store+instance pair, creating a fresh one if the pool is empty.
    fn checkout(&self) -> NuResult<PooledInstance> {
        if let Some(mut pooled) = self
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
        {
            pooled.store.data_mut().log_messages.clear();
            pooled
                .store
                .set_fuel(self.limits.fuel_per_call)
                .map_err(|e| NuError::VMError {
                    msg: format!("wasmtime fuel reset: {}", e),
                    span: Span::default(),
                })?;
            return Ok(pooled);
        }
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.memory_bytes)
            .instances(self.limits.max_store_instances)
            .memories(self.limits.max_memories)
            .tables(self.limits.max_tables)
            .trap_on_grow_failure(true)
            .build();
        let mut store = Store::new(
            &self.engine,
            HostState {
                caps: self.caps,
                log_messages: Vec::new(),
                limits: store_limits,
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel_per_call)
            .map_err(|e| NuError::VMError {
                msg: format!("wasmtime fuel: {}", e),
                span: Span::default(),
            })?;
        let instance = self
            .linker
            .instantiate(&mut store, &self.component)
            .map_err(|e| NuError::VMError {
                msg: format!("wasmtime instantiate: {}", e),
                span: Span::default(),
            })?;
        Ok(PooledInstance { store, instance })
    }

    /// Return a store+instance pair to the pool for reuse.
    fn checkin(&self, pooled: PooledInstance) {
        let mut pool = self
            .pool
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pool.len() < self.limits.max_idle_instances {
            pool.push(pooled);
        }
    }

    fn with_actor<F, R>(&self, f: F) -> NuResult<R>
    where
        F: FnOnce(&mut Store<HostState>, &Actor) -> NuResult<R>,
    {
        let mut pooled = self.checkout()?;
        let actor =
            Actor::new(&mut pooled.store, &pooled.instance).map_err(|e| NuError::VMError {
                msg: format!("wasmtime actor bindings: {}", e),
                span: Span::default(),
            })?;
        let result = f(&mut pooled.store, &actor);
        // A trap may leave guest state partially mutated. Never return that
        // instance to another actor invocation.
        if result.is_ok() {
            self.checkin(pooled);
        }
        result
    }

    pub fn init(&self) -> NuResult<i64> {
        self.with_actor(|store, actor| {
            actor.call_init(store).map_err(|e| NuError::VMError {
                msg: format!("wasmtime call_init: {}", e),
                span: Span::default(),
            })
        })
    }

    pub fn handle_message(&self, msg: &[u8]) -> NuResult<i64> {
        self.with_actor(|store, actor| {
            actor
                .call_handle_message(store, msg)
                .map_err(|e| NuError::VMError {
                    msg: format!("wasmtime call_handle_message: {}", e),
                    span: Span::default(),
                })
        })
    }

    pub fn checkpoint(&self) -> NuResult<Vec<u8>> {
        self.with_actor(|store, actor| {
            actor.call_checkpoint(store).map_err(|e| NuError::VMError {
                msg: format!("wasmtime call_checkpoint: {}", e),
                span: Span::default(),
            })
        })
    }
}

#[cfg(test)]
#[cfg(feature = "wasm-backend")]
mod tests {
    use super::*;

    /// Minimal WAT component that imports `log` and exports `init`.
    const LOG_IMPORT_WAT: &str = r#"
        (component
            (import "nulang:runtime/host" (instance $host
                (export "log" (func (param "msg" string)))
            ))
        )
    "#;

    #[test]
    fn test_component_capability_gate_denies_log() {
        let wasm = wat::parse_str(LOG_IMPORT_WAT).expect("parse WAT");
        let rt = ComponentRuntime::new_with_caps(
            &wasm,
            Capabilities {
                allow_log: false,
                allow_clock: false,
                allow_random: false,
            },
        )
        .expect("new_with_caps");

        // Direct instantiation with the linker should fail because the host
        // interface is not added when allow_log is false.
        let engine = wasmtime::Engine::new(&component_config()).expect("engine");
        let mut store = wasmtime::Store::new(
            &engine,
            HostState {
                caps: rt.caps,
                log_messages: Vec::new(),
                limits: StoreLimitsBuilder::new().build(),
            },
        );
        let linker = ComponentRuntime::build_linker(&engine, rt.caps).expect("linker");
        let component = wasmtime::component::Component::new(&engine, &wasm).expect("component");
        let err = linker
            .instantiate(&mut store, &component)
            .expect_err("should fail to instantiate without log capability");
        assert!(
            err.to_string().contains("host"),
            "error should mention missing host import: {}",
            err
        );
    }

    #[test]
    fn test_component_capability_gate_allows_log() {
        let wasm = wat::parse_str(LOG_IMPORT_WAT).expect("parse WAT");
        let rt = ComponentRuntime::new_with_caps(
            &wasm,
            Capabilities {
                allow_log: true,
                allow_clock: false,
                allow_random: false,
            },
        )
        .expect("new_with_caps");

        // Direct instantiation with the linker should succeed because the host
        // interface is added when allow_log is true.
        let engine = wasmtime::Engine::new(&component_config()).expect("engine");
        let mut store = wasmtime::Store::new(
            &engine,
            HostState {
                caps: rt.caps,
                log_messages: Vec::new(),
                limits: StoreLimitsBuilder::new().build(),
            },
        );
        let linker = ComponentRuntime::build_linker(&engine, rt.caps).expect("linker");
        let component = wasmtime::component::Component::new(&engine, &wasm).expect("component");
        let _instance = linker
            .instantiate(&mut store, &component)
            .expect("should succeed with log capability");
    }

    #[test]
    fn test_component_config_fuel_traps_runaway_guest() {
        let engine = Engine::new(&component_config()).expect("engine");
        let module = Module::new(
            &engine,
            r#"(module
                (func (export "spin")
                    (loop $forever
                        br $forever)))"#,
        )
        .expect("module");
        let mut store = Store::new(
            &engine,
            HostState {
                caps: Capabilities::default(),
                log_messages: Vec::new(),
                limits: StoreLimitsBuilder::new().build(),
            },
        );
        store.set_fuel(1_000).expect("fuel");
        let instance = wasmtime::Instance::new(&mut store, &module, &[]).expect("instance");
        let spin = instance
            .get_typed_func::<(), ()>(&mut store, "spin")
            .expect("spin");

        let err = spin
            .call(&mut store, ())
            .expect_err("runaway guest must exhaust fuel");
        assert!(
            err.to_string().contains("fuel"),
            "trap should identify fuel exhaustion: {err}"
        );
    }

    #[test]
    fn test_component_store_limits_memory_growth() {
        let engine = Engine::new(&component_config()).expect("engine");
        let module = Module::new(&engine, r#"(module (memory 2))"#).expect("module");
        let mut store = Store::new(
            &engine,
            HostState {
                caps: Capabilities::default(),
                log_messages: Vec::new(),
                limits: StoreLimitsBuilder::new().memory_size(64 << 10).build(),
            },
        );
        store.limiter(|state| &mut state.limits);

        wasmtime::Instance::new(&mut store, &module, &[])
            .err()
            .expect("two-page memory must exceed the one-page store limit");
    }

    #[test]
    fn test_component_policy_rejects_zero_resource_limits() {
        let wasm = wat::parse_str("(component)").expect("parse WAT");
        let err = ComponentRuntime::new_with_policy(
            &wasm,
            Capabilities::default(),
            ComponentLimits {
                fuel_per_call: 0,
                ..ComponentLimits::default()
            },
        )
        .err()
        .expect("zero fuel must be rejected");
        assert!(err.to_string().contains("fuel_per_call"));

        let err = ComponentRuntime::new_with_policy(
            &wasm,
            Capabilities::default(),
            ComponentLimits {
                memory_bytes: 0,
                ..ComponentLimits::default()
            },
        )
        .err()
        .expect("zero memory limit must be rejected");
        assert!(err.to_string().contains("memory_bytes"));
    }
}
