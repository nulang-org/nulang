use nulang::bytecode::{CodeModule, Constant, Instruction, OpCode};
use nulang::iso_arena::IsoArena;
use nulang::runtime::heap::{ActorHeap, TypeTag};
use nulang::runtime::OrcaGc;
use nulang::vm::{ActorVmCallbacks, Value, VM};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Debug, Default)]
struct AllocStats {
    heap_allocs: usize,
    arena_allocs: usize,
    arena_resets: usize,
    heap_ref_drops: usize,
    arena_ref_drops: usize,
}

#[derive(Debug)]
struct TrackingCallbacks {
    heap: ActorHeap,
    arena: IsoArena,
    gc: OrcaGc,
    stats: Rc<RefCell<AllocStats>>,
}

impl TrackingCallbacks {
    fn new(stats: Rc<RefCell<AllocStats>>) -> Self {
        let mut heap = ActorHeap::new(64 * 1024);
        heap.set_actor_id(0);
        Self {
            heap,
            arena: IsoArena::new(),
            gc: OrcaGc::new(0),
            stats,
        }
    }
}

impl ActorVmCallbacks for TrackingCallbacks {
    fn alloc(&mut self, size: usize, type_tag: TypeTag) -> Option<*mut u8> {
        self.stats.borrow_mut().heap_allocs += 1;
        self.heap.alloc(size, type_tag)
    }

    fn alloc_arena(&mut self, size: usize, type_tag: TypeTag) -> Option<*mut u8> {
        self.stats.borrow_mut().arena_allocs += 1;
        self.arena.alloc(size, type_tag)
    }

    fn reset_arena(&mut self) {
        self.stats.borrow_mut().arena_resets += 1;
        self.arena.reset();
    }

    fn is_arena_ptr(&self, ptr: *const u8) -> bool {
        self.arena.contains(ptr)
    }

    fn drop_ref(&mut self, ptr: *mut u8) {
        if self.arena.contains(ptr) {
            self.stats.borrow_mut().arena_ref_drops += 1;
            return;
        }
        self.stats.borrow_mut().heap_ref_drops += 1;
        unsafe {
            self.gc.drop_local_ref(&mut self.heap, ptr);
        }
    }

    fn retain_ref(&mut self, ptr: *mut u8) {
        if !self.arena.contains(ptr) {
            unsafe {
                self.gc.local_ref(&self.heap, ptr);
            }
        }
    }

    fn array_len(&self, ptr: *mut u8) -> Option<usize> {
        if ptr.is_null() {
            return None;
        }
        unsafe {
            let header = &*ActorHeap::header_of(ptr);
            (header.type_tag == TypeTag::Array).then(|| {
                header.size.saturating_sub(ActorHeap::HEADER_SIZE) / std::mem::size_of::<Value>()
            })
        }
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
}

fn with_iso_arena_vm() -> (VM, Rc<RefCell<AllocStats>>) {
    let mut vm = VM::new_without_jit();
    vm.set_iso_arena_enabled(true);
    let stats = Rc::new(RefCell::new(AllocStats::default()));
    (vm, stats)
}

fn local_array_module(escapes: bool) -> CodeModule {
    let mut module = CodeModule::new("iso-arena-routing");
    let len = module.add_constant(Constant::Int(4));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((len >> 8) & 0xff) as u8,
        (len & 0xff) as u8,
        1,
    ));
    let dst = if escapes { 0 } else { 2 };
    module.emit(Instruction::new2(OpCode::ArrAlloc, 1, dst));
    if !escapes {
        module.emit(Instruction::new1(OpCode::Drop, dst));
    }
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);
    module
}

fn local_fixed_composite_module(opcode: OpCode) -> CodeModule {
    let mut module = CodeModule::new("iso-arena-fixed-composite-routing");
    module.emit(Instruction::new2(opcode, 2, 2));
    module.emit(Instruction::new1(OpCode::Drop, 2));
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);
    module
}

#[test]
fn qualifying_record_allocation_uses_iso_arena() {
    let (mut vm, stats) = with_iso_arena_vm();
    vm.set_actor_callbacks(Box::new(TrackingCallbacks::new(stats.clone())));
    vm.load_module(local_fixed_composite_module(OpCode::RecMk));

    vm.run().expect("VM run should succeed");

    let stats = stats.borrow();
    assert_eq!(stats.arena_allocs, 1);
    assert_eq!(stats.heap_allocs, 0);
    assert_eq!(stats.arena_resets, 1);
}

#[test]
fn qualifying_tuple_allocation_uses_iso_arena() {
    let (mut vm, stats) = with_iso_arena_vm();
    vm.set_actor_callbacks(Box::new(TrackingCallbacks::new(stats.clone())));
    vm.load_module(local_fixed_composite_module(OpCode::TupleMk));

    vm.run().expect("VM run should succeed");

    let stats = stats.borrow();
    assert_eq!(stats.arena_allocs, 1);
    assert_eq!(stats.heap_allocs, 0);
    assert_eq!(stats.arena_resets, 1);
}

#[test]
fn qualifying_allocation_uses_iso_arena_and_resets_at_completion() {
    let (mut vm, stats) = with_iso_arena_vm();
    vm.set_actor_callbacks(Box::new(TrackingCallbacks::new(stats.clone())));
    vm.load_module(local_array_module(false));

    vm.run().expect("VM run should succeed");

    let stats = stats.borrow();
    assert_eq!(
        stats.arena_allocs, 1,
        "qualifying allocation must use arena"
    );
    assert_eq!(
        stats.heap_allocs, 0,
        "qualifying allocation must bypass heap"
    );
    assert_eq!(
        stats.arena_resets, 1,
        "completed activation must reset arena"
    );
}

#[test]
fn escaping_allocation_stays_on_heap() {
    let (mut vm, stats) = with_iso_arena_vm();
    vm.set_actor_callbacks(Box::new(TrackingCallbacks::new(stats.clone())));
    vm.load_module(local_array_module(true));

    vm.run().expect("VM run should succeed");

    let stats = stats.borrow();
    assert_eq!(
        stats.arena_allocs, 0,
        "escaping allocation must not use arena"
    );
    assert_eq!(
        stats.heap_allocs, 1,
        "escaping allocation must remain heap-backed"
    );
    assert_eq!(
        stats.arena_resets, 1,
        "completed activation still resets arena"
    );
}

#[test]
fn disabled_iso_arena_keeps_local_allocations_heap_backed() {
    let (mut vm, stats) = with_iso_arena_vm();
    vm.set_iso_arena_enabled(false);
    vm.set_actor_callbacks(Box::new(TrackingCallbacks::new(stats.clone())));
    vm.load_module(local_array_module(false));

    vm.run().expect("VM run should succeed");

    let stats = stats.borrow();
    assert_eq!(stats.heap_allocs, 1);
    assert_eq!(stats.arena_allocs, 0);
    assert_eq!(stats.arena_resets, 0);
}

#[test]
fn enabling_iso_arena_after_module_load_recomputes_allocation_sites() {
    let mut vm = VM::new_without_jit();
    vm.set_iso_arena_enabled(false);
    let stats = Rc::new(RefCell::new(AllocStats::default()));
    vm.set_actor_callbacks(Box::new(TrackingCallbacks::new(stats.clone())));
    vm.load_module(local_array_module(false));
    vm.set_iso_arena_enabled(true);

    vm.run().expect("VM run should succeed");

    let stats = stats.borrow();
    assert_eq!(stats.heap_allocs, 0);
    assert_eq!(stats.arena_allocs, 1);
    assert_eq!(stats.arena_resets, 1);
}

#[test]
fn yielding_activation_preserves_arena_until_resume_completes() {
    let (mut vm, stats) = with_iso_arena_vm();
    vm.set_actor_callbacks(Box::new(TrackingCallbacks::new(stats.clone())));
    let mut module = local_array_module(false);
    let last = module.instructions.len() - 1;
    module
        .instructions
        .insert(last, Instruction::new0(OpCode::Yield));
    vm.load_module(module);

    vm.run_from(0, 0).expect("first segment should yield");
    assert!(vm.yield_pending);
    {
        let counts = stats.borrow();
        assert_eq!(counts.arena_allocs, 1);
        assert_eq!(
            counts.arena_resets, 0,
            "yield must not reclaim activation arena"
        );
    }

    vm.resume().expect("resumed segment should complete");
    assert!(!vm.yield_pending);
    let counts = stats.borrow();
    assert_eq!(counts.arena_resets, 1, "completion reclaims the arena once");
}

#[test]
fn disabled_arena_local_drop_releases_through_orca() {
    let (mut vm, stats) = with_iso_arena_vm();
    vm.set_iso_arena_enabled(false);
    vm.set_actor_callbacks(Box::new(TrackingCallbacks::new(stats.clone())));
    vm.load_module(local_array_module(false));

    vm.run().expect("normal heap-backed run should succeed");

    let stats = stats.borrow();
    assert_eq!(stats.heap_allocs, 1);
    assert_eq!(stats.heap_ref_drops, 1);
    assert_eq!(stats.arena_ref_drops, 0);
}

#[test]
fn qualified_arena_drop_never_enters_orca() {
    let (mut vm, stats) = with_iso_arena_vm();
    vm.set_actor_callbacks(Box::new(TrackingCallbacks::new(stats.clone())));
    vm.load_module(local_array_module(false));

    vm.run().expect("arena-backed run should succeed");

    let stats = stats.borrow();
    assert_eq!(stats.arena_allocs, 1);
    assert_eq!(stats.arena_ref_drops, 1);
    assert_eq!(stats.heap_ref_drops, 0);
    assert_eq!(stats.arena_resets, 1);
}
