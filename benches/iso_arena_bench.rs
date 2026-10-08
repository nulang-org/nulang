//! Interpreter-only A/B: same transient array workload with the optional
//! activation-local arena off (ORCA heap) or on (iso arena).
//!
//! Run: cargo bench --bench bench_main -- vm/iso_arena
//! All VM construction, bytecode loading, and static escape classification
//! happen outside the timed region. Only VM execution is measured.

use criterion::{black_box, criterion_group, BatchSize, BenchmarkId, Criterion, Throughput};
use nulang::bytecode::{CodeModule, Constant, Instruction, OpCode};
use nulang::iso_arena::IsoArena;
use nulang::runtime::heap::{ActorHeap, TypeTag};
use nulang::vm::{ActorVmCallbacks, Value, VM};

const ALLOCATIONS: usize = 256;
const ARRAY_ELEMENTS: usize = 8;

struct BenchCallbacks {
    heap: ActorHeap,
    arena: IsoArena,
}

impl BenchCallbacks {
    fn new() -> Self {
        Self {
            heap: ActorHeap::new(64 * 1024),
            arena: IsoArena::new(),
        }
    }
}

impl ActorVmCallbacks for BenchCallbacks {
    fn alloc(&mut self, size: usize, type_tag: TypeTag) -> Option<*mut u8> {
        self.heap.alloc(size, type_tag)
    }

    fn alloc_arena(&mut self, size: usize, type_tag: TypeTag) -> Option<*mut u8> {
        self.arena.alloc(size, type_tag)
    }

    fn reset_arena(&mut self) {
        self.arena.reset();
    }

    fn is_arena_ptr(&self, ptr: *const u8) -> bool {
        self.arena.contains(ptr)
    }

    fn drop_ref(&mut self, _ptr: *mut u8) {}

    fn retain_ref(&mut self, _ptr: *mut u8) {}

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

fn transient_arrays_module() -> CodeModule {
    let mut module = CodeModule::new("iso-arena-benchmark");
    let constant = module.add_constant(Constant::Int(ARRAY_ELEMENTS as i64));
    module.emit(Instruction::new3(
        OpCode::ConstU,
        ((constant >> 8) & 0xff) as u8,
        (constant & 0xff) as u8,
        1,
    ));
    for _ in 0..ALLOCATIONS {
        module.emit(Instruction::new2(OpCode::ArrAlloc, 1, 2));
        module.emit(Instruction::new1(OpCode::Drop, 2));
    }
    module.emit(Instruction::new0(OpCode::Halt));
    module.entry_point = Some(0);
    module
}

fn fresh_vm(module: &CodeModule, arena: bool) -> VM {
    let mut vm = VM::new_without_jit();
    vm.set_iso_arena_enabled(arena);
    vm.set_actor_callbacks(Box::new(BenchCallbacks::new()));
    vm.load_module(module.clone());
    vm
}

fn bench_iso_arena(c: &mut Criterion) {
    let module = transient_arrays_module();
    assert_eq!(
        nulang::iso_arena::qualifying_alloc_sites(&module).len(),
        ALLOCATIONS,
        "benchmark must actually exercise the safe arena allocation sites"
    );

    let mut group = c.benchmark_group("vm/iso_arena");
    group.throughput(Throughput::Elements(ALLOCATIONS as u64));
    for (name, enabled) in [("off_orca_heap", false), ("on_iso_arena", true)] {
        group.bench_with_input(BenchmarkId::new("transient_arrays_256", name), &enabled, |b, &on| {
            b.iter_batched_ref(
                || fresh_vm(&module, on),
                |vm| black_box(vm.run().expect("identical VM workload must complete")),
                BatchSize::SmallInput,
            )
        });
    }
    group.finish();
}

criterion_group!(benches, bench_iso_arena);
