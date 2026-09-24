use nulang::bytecode::{
    CodeModule, Constant, EffectSiteMetadata, Instruction, OpCode,
};
use nulang::runtime::heap::TypeTag as HeapTypeTag;
use nulang::vm::{
    ActorVmCallbacks, EffectInvocationContext, PerformAsyncResult, VM, Value,
};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct Seen {
    builtin: Option<(usize, [u8; 32], String)>,
    asynchronous: Option<(usize, [u8; 32], String)>,
}

#[derive(Debug)]
struct CaptureCallbacks {
    seen: Arc<Mutex<Seen>>,
}

impl ActorVmCallbacks for CaptureCallbacks {
    fn alloc(&mut self, _size: usize, _type_tag: HeapTypeTag) -> Option<*mut u8> {
        None
    }

    fn drop_ref(&mut self, _ptr: *mut u8) {}

    fn retain_ref(&mut self, _ptr: *mut u8) {}

    fn array_len(&self, _ptr: *mut u8) -> Option<usize> {
        None
    }

    fn spawn_actor(
        &mut self,
        _module: &CodeModule,
        _spawn_pc: usize,
        _behavior_idx: usize,
        _init: Vec<(String, Value)>,
    ) -> Value {
        Value::nil()
    }

    fn send_message(&mut self, _target: Value, _behavior_id: u16, _args: &[Value]) {}

    fn perform_builtin_effect_at_site(
        &mut self,
        context: EffectInvocationContext<'_>,
        effect_name: &str,
        op_name: Option<&str>,
        _module: &CodeModule,
        _regs: &[Value],
    ) -> Option<Value> {
        let site = context.site.expect("compiler-owned site metadata");
        self.seen.lock().unwrap().builtin = Some((
            context.pc,
            site.id,
            format!("{}.{}", effect_name, op_name.unwrap_or_default()),
        ));
        Some(Value::unit())
    }

    fn perform_async_at_site(
        &mut self,
        context: EffectInvocationContext<'_>,
        effect_op: &str,
        _constants: &[Constant],
        _args: &[Value],
    ) -> PerformAsyncResult {
        let site = context.site.expect("compiler-owned site metadata");
        self.seen.lock().unwrap().asynchronous =
            Some((context.pc, site.id, effect_op.to_string()));
        PerformAsyncResult::Ready(None)
    }
}

fn module_with_effect(opcode: OpCode, operation: &str, id: [u8; 32]) -> CodeModule {
    let mut module = CodeModule::new("effect-context-test");
    let effect_idx = module.add_constant(Constant::String(operation.to_string()));
    module.emit(Instruction::new3(
        opcode,
        ((effect_idx >> 8) & 0xff) as u8,
        (effect_idx & 0xff) as u8,
        0,
    ));
    module.emit(Instruction::new0(OpCode::Halt));
    module.effect_sites.push(EffectSiteMetadata {
        pc: 0,
        id,
        effect_operation: operation.to_string(),
    });
    module.entry_point = Some(0);
    module
}

#[test]
fn perform_callback_receives_exact_semantic_site_context() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let callbacks = CaptureCallbacks { seen: seen.clone() };
    let mut vm = VM::new_without_jit();
    vm.load_module(module_with_effect(
        OpCode::Perform,
        "Host.echo",
        [0x11; 32],
    ));
    vm.set_actor_callbacks(Box::new(callbacks));

    vm.run().expect("effect should be handled");

    assert_eq!(
        seen.lock().unwrap().builtin,
        Some((0, [0x11; 32], "Host.echo".to_string()))
    );
}

#[test]
fn perform_async_callback_receives_exact_semantic_site_context() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let callbacks = CaptureCallbacks { seen: seen.clone() };
    let mut vm = VM::new_without_jit();
    vm.load_module(module_with_effect(
        OpCode::PerformAsync,
        "Inference.ask",
        [0x22; 32],
    ));
    vm.set_actor_callbacks(Box::new(callbacks));

    vm.run().expect("async effect should resolve synchronously");

    assert_eq!(
        seen.lock().unwrap().asynchronous,
        Some((0, [0x22; 32], "Inference.ask".to_string()))
    );
}
