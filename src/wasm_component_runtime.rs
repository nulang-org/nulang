use crate::types::{NuError, NuResult, Span};

#[cfg(feature = "wasm-backend")]
use crate::authority::{AuthorityGrant, AuthorityManifest};
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
    config.memory_reservation(4 << 30);
    config.memory_guard_size(128 << 20);
    config.cranelift_opt_level(OptLevel::Speed);
    config.wasm_simd(true);
    config
}

/// Exact authority grant required for one low-level WASM host import.
///
/// These use the existing typed authority extension point rather than a
/// backend-specific boolean policy. The WIT host functions are intentionally
/// narrow: source-level effects may lower to these operations, but granting one
/// operation never grants a sibling operation.
#[cfg(feature = "wasm-backend")]
fn component_host_grant(namespace: &str, operation: &str) -> AuthorityGrant {
    AuthorityGrant::Other {
        namespace: namespace.to_string(),
        operation: operation.to_string(),
        argument: None,
    }
}

#[cfg(feature = "wasm-backend")]
fn allows_component_host(
    authority: &AuthorityManifest,
    namespace: &str,
    operation: &str,
) -> bool {
    authority.allows(&component_host_grant(namespace, operation))
}

#[cfg(feature = "wasm-backend")]
pub struct HostState {
    authority: AuthorityManifest,
    log_messages: Vec<String>,
}

#[cfg(feature = "wasm-backend")]
impl HostTrait for HostState {
    fn log(&mut self, msg: String) {
        assert!(
            allows_component_host(&self.authority, "IO", "Log"),
            "WASM host authority denied: IO::Log"
        );
        self.log_messages.push(msg);
    }

    fn clock_now(&mut self) -> u64 {
        assert!(
            allows_component_host(&self.authority, "Time", "Now"),
            "WASM host authority denied: Time::Now"
        );
        0
    }

    fn random_u64(&mut self) -> u64 {
        assert!(
            allows_component_host(&self.authority, "Random", "U64"),
            "WASM host authority denied: Random::U64"
        );
        0
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
    authority: AuthorityManifest,
    pool: std::sync::Mutex<Vec<PooledInstance>>,
}

#[cfg(feature = "wasm-backend")]
impl ComponentRuntime {
    /// Construct a deny-by-default component runtime.
    pub fn new(wasm_bytes: &[u8]) -> NuResult<Self> {
        Self::new_with_authority(wasm_bytes, AuthorityManifest::new())
    }

    /// Construct a component runtime with an exact typed authority manifest.
    ///
    /// Only WIT host functions whose exact grant is present are linked. A
    /// component importing any other host function fails instantiation.
    pub fn new_with_authority(
        wasm_bytes: &[u8],
        authority: AuthorityManifest,
    ) -> NuResult<Self> {
        let config = component_config();
        let engine = Engine::new(&config).map_err(|e| NuError::VMError {
            msg: format!("wasmtime engine: {}", e),
            span: Span::default(),
        })?;
        let component = Component::new(&engine, wasm_bytes).map_err(|e| NuError::VMError {
            msg: format!("wasmtime component: {}", e),
            span: Span::default(),
        })?;
        let linker = Self::build_linker(&engine, &authority)?;
        Ok(ComponentRuntime {
            engine,
            component,
            linker,
            authority,
            pool: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn build_linker(
        engine: &Engine,
        authority: &AuthorityManifest,
    ) -> NuResult<wasmtime::component::Linker<HostState>> {
        let mut linker = wasmtime::component::Linker::<HostState>::new(engine);
        let allow_log = allows_component_host(authority, "IO", "Log");
        let allow_clock = allows_component_host(authority, "Time", "Now");
        let allow_random = allows_component_host(authority, "Random", "U64");

        // Add only the exact host operations authorized by the manifest.
        if allow_log || allow_clock || allow_random {
            let mut inst =
                linker
                    .instance("nulang:runtime/host")
                    .map_err(|e| NuError::VMError {
                        msg: format!("wasmtime linker: {}", e),
                        span: Span::default(),
                    })?;
            if allow_log {
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
            if allow_clock {
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
            if allow_random {
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
        if let Some(mut pooled) = self.pool.lock().unwrap().pop() {
            pooled.store.data_mut().log_messages.clear();
            return Ok(pooled);
        }
        let mut store = Store::new(
            &self.engine,
            HostState {
                authority: self.authority.clone(),
                log_messages: Vec::new(),
            },
        );
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
        self.pool.lock().unwrap().push(pooled);
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
        self.checkin(pooled);
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

    /// Minimal WAT component that imports `log`.
    const LOG_IMPORT_WAT: &str = r#"
        (component
            (import "nulang:runtime/host" (instance $host
                (export "log" (func (param "msg" string)))
            ))
        )
    "#;

    fn manifest(grants: &[(&str, &str)]) -> AuthorityManifest {
        AuthorityManifest::from_grants(grants.iter().map(|(namespace, operation)| {
            component_host_grant(namespace, operation)
        }))
    }

    #[test]
    fn component_authority_is_deny_by_default() {
        let wasm = wat::parse_str(LOG_IMPORT_WAT).expect("parse WAT");
        let rt = ComponentRuntime::new(&wasm).expect("new runtime");
        let engine = wasmtime::Engine::new(&component_config()).expect("engine");
        let mut store = wasmtime::Store::new(
            &engine,
            HostState {
                authority: AuthorityManifest::new(),
                log_messages: Vec::new(),
            },
        );
        let linker =
            ComponentRuntime::build_linker(&engine, &rt.authority).expect("linker");
        let component = wasmtime::component::Component::new(&engine, &wasm).expect("component");
        let err = linker
            .instantiate(&mut store, &component)
            .expect_err("missing IO::Log authority must deny the import");
        assert!(
            err.to_string().contains("host"),
            "error should mention missing host import: {}",
            err
        );
    }

    #[test]
    fn exact_component_authority_allows_log() {
        let wasm = wat::parse_str(LOG_IMPORT_WAT).expect("parse WAT");
        let authority = manifest(&[("IO", "Log")]);
        let rt = ComponentRuntime::new_with_authority(&wasm, authority.clone())
            .expect("new_with_authority");
        let engine = wasmtime::Engine::new(&component_config()).expect("engine");
        let mut store = wasmtime::Store::new(
            &engine,
            HostState {
                authority,
                log_messages: Vec::new(),
            },
        );
        let linker =
            ComponentRuntime::build_linker(&engine, &rt.authority).expect("linker");
        let component = wasmtime::component::Component::new(&engine, &wasm).expect("component");
        let _instance = linker
            .instantiate(&mut store, &component)
            .expect("exact IO::Log authority should link log");
    }

    #[test]
    fn sibling_authority_does_not_authorize_log() {
        let wasm = wat::parse_str(LOG_IMPORT_WAT).expect("parse WAT");
        let authority = manifest(&[("Time", "Now")]);
        let rt = ComponentRuntime::new_with_authority(&wasm, authority.clone())
            .expect("new_with_authority");
        let engine = wasmtime::Engine::new(&component_config()).expect("engine");
        let mut store = wasmtime::Store::new(
            &engine,
            HostState {
                authority,
                log_messages: Vec::new(),
            },
        );
        let linker =
            ComponentRuntime::build_linker(&engine, &rt.authority).expect("linker");
        let component = wasmtime::component::Component::new(&engine, &wasm).expect("component");
        linker
            .instantiate(&mut store, &component)
            .expect_err("Time::Now must not authorize IO::Log");
    }

    #[test]
    fn component_host_grants_are_exact_typed_authority() {
        let authority = manifest(&[("IO", "Log"), ("Random", "U64")]);
        assert!(authority.allows(&component_host_grant("IO", "Log")));
        assert!(authority.allows(&component_host_grant("Random", "U64")));
        assert!(!authority.allows(&component_host_grant("Time", "Now")));
        assert!(!authority.allows(&component_host_grant("IO", "Print")));
    }
}
