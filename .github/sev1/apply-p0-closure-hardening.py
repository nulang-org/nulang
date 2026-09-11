#!/usr/bin/env python3
from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(
            f"{path}: expected exactly one match, found {count}: {old[:120]!r}"
        )
    p.write_text(text.replace(old, new, 1))


# Unary negation can observe a dynamically non-numeric/nil value even in a
# function whose nominal return type is Int. Keep such functions boxed so the
# checked helper remains authoritative.
replace_once(
    "src/aot/codegen.rs",
    '''                    mir::RValue::Binary(
                        crate::ast::BinOp::Div | crate::ast::BinOp::Mod | crate::ast::BinOp::Pow,
                        ..
                    ) | mir::RValue::ArrayLit(_)
                        | mir::RValue::ArrayLoad { .. }
''',
    '''                    mir::RValue::Binary(
                        crate::ast::BinOp::Div | crate::ast::BinOp::Mod | crate::ast::BinOp::Pow,
                        ..
                    ) | mir::RValue::Unary(crate::ast::UnOp::Neg, ..)
                        | mir::RValue::ArrayLit(_)
                        | mir::RValue::ArrayLoad { .. }
''',
)

# Zero-capture closures are canonical immediate TAG_CLOSURE values. Do not
# heap-allocate them and do not encode them as numeric TAG_INT values.
replace_once(
    "src/aot/codegen.rs",
    '''        mir::RValue::Closure { func, captures } => {
            if captures.is_empty() {
                // A first-class closure must not masquerade as a numeric TAG_INT.
                // Static direct-call metadata still resolves this target without
                // dynamic dispatch; the runtime value uses canonical TAG_CLOSURE.
                let fn_val = builder.ins().iconst(types::I64, *func as i64);
                let helper = helpers.get("nulang_aot_make_closure_0").ok_or_else(|| {
                    AotCompileError::Internal("missing nulang_aot_make_closure_0 helper".into())
                })?;
                let call = builder.ins().call(*helper, &[fn_val]);
                Ok(builder.inst_results(call)[0])
            } else {
''',
    '''        mir::RValue::Closure { func, captures } => {
            if captures.is_empty() {
                // Immediate closure representation is shared with the VM:
                // TAG_CLOSURE with the function index in the 48-bit payload.
                // Encoding this as TAG_INT makes a function value numeric;
                // heap-allocating it makes immediate/dynamic closure semantics diverge.
                let idx = builder.ins().iconst(types::I64, *func as i64);
                let mask = builder
                    .ins()
                    .iconst(types::I64, crate::value_layout::PAYLOAD_MASK as i64);
                let payload = builder.ins().band(idx, mask);
                let tag = builder
                    .ins()
                    .iconst(types::I64, crate::value_layout::TAG_CLOSURE as i64);
                Ok(builder.ins().bor(payload, tag))
            } else {
''',
)

# Captured closure payloads are byte-sized allocations; validate pointer fit
# before tagging and mask the pointer into the 48-bit payload.
replace_once(
    "src/jit/runtime.rs",
    '''            let count: usize = 0 $(+ { let _ = stringify!($cap); 1 })*;
            let Some(ptr) = alloc_obj((2 + count) * std::mem::size_of::<u64>(), HeapTypeTag::Closure) else {
                return Value::nil().as_raw();
            };
            let slot = ptr as *mut u64;
''',
    '''            let count: usize = 0 $(+ { let _ = stringify!($cap); 1 })*;
            let payload_size = (2 + count) * std::mem::size_of::<u64>();
            let Some(ptr) = alloc_obj(payload_size, HeapTypeTag::Closure) else {
                return Value::nil().as_raw();
            };
            debug_assert!(
                crate::value_layout::ptr_fits_payload(ptr as u64),
                "AOT closure pointer exceeds 48-bit value payload"
            );
            let slot = ptr as *mut u64;
''',
)

replace_once(
    "src/jit/runtime.rs",
    '''            (TAG_CLOSURE | ptr as u64)
''',
    '''            TAG_CLOSURE | ((ptr as u64) & PAYLOAD_MASK)
''',
)

# Dynamic calls must accept the canonical immediate closure representation and
# validate captured closure heap metadata before reading captures.
replace_once(
    "src/jit/runtime.rs",
    '''        /// Invoke a closure value: an uncaptured closure is a tagged fn index
        /// (dispatch with the explicit args only); a captured closure is a
        /// TAG_CLOSURE object carrying fn_idx + captures (dispatch with
        /// args + captures). Handles closures whose target is not statically
        /// known at the call site (e.g. passed as a parameter).
        #[no_mangle]
        pub unsafe extern "C" fn $name(closure_raw: u64 $(, $arg: u64)*) -> u64 {
            let args = [$($arg),*];
            let fn_ptr;
            let mut all: Vec<u64>;
            if (closure_raw & TAG_MASK) == TAG_INT {
                // Uncaptured closure: the tagged payload is the fn index.
                let fn_idx = (closure_raw & PAYLOAD_MASK) as i64;
                fn_ptr = crate::aot::nulang_aot_resolve_fn(fn_idx as u64);
                all = Vec::with_capacity(args.len());
                all.extend_from_slice(&args);
            } else if (closure_raw & TAG_MASK) == TAG_CLOSURE {
                // Captured closure object: [fn_idx, cap_count, cap0..].
                let ptr = (closure_raw & PAYLOAD_MASK) as *mut u64;
                if ptr.is_null() {
                    return Value::nil().as_raw();
                }
                let fn_idx = *ptr;
                let cap_count = *ptr.add(1) as usize;
                fn_ptr = crate::aot::nulang_aot_resolve_fn(fn_idx);
                all = Vec::with_capacity(args.len() + cap_count);
                all.extend_from_slice(&args);
                for i in 0..cap_count {
                    all.push(*ptr.add(2 + i));
                }
            } else {
                return Value::nil().as_raw();
            }
''',
    '''        /// Invoke a closure value. Uncaptured closures use the VM's
        /// immediate representation (TAG_CLOSURE + function-index payload).
        /// Captured AOT closures use TAG_CLOSURE + a heap-object pointer whose
        /// payload is [fn_idx, cap_count, cap0..]. Handles closure values whose
        /// target is not statically known at the call site.
        #[no_mangle]
        pub unsafe extern "C" fn $name(closure_raw: u64 $(, $arg: u64)*) -> u64 {
            let args = [$($arg),*];
            if (closure_raw & TAG_MASK) != TAG_CLOSURE {
                return Value::nil().as_raw();
            }

            let payload = closure_raw & PAYLOAD_MASK;
            let immediate_fn = crate::aot::nulang_aot_resolve_fn(payload);
            let fn_ptr;
            let mut all: Vec<u64>;
            if immediate_fn != 0 {
                // Canonical zero-capture closure: payload is the function idx.
                fn_ptr = immediate_fn;
                all = Vec::with_capacity(args.len());
                all.extend_from_slice(&args);
            } else {
                // AOT captured closure object: [fn_idx, cap_count, cap0..].
                let ptr = payload as *mut u64;
                if ptr.is_null() {
                    return Value::nil().as_raw();
                }
                let header = &*ActorHeap::header_of(ptr as *mut u8);
                if header.type_tag != HeapTypeTag::Closure {
                    return Value::nil().as_raw();
                }
                let payload_size = header.size.saturating_sub(ActorHeap::HEADER_SIZE);
                if payload_size < 2 * std::mem::size_of::<u64>() {
                    return Value::nil().as_raw();
                }
                let fn_idx = *ptr;
                let cap_count = *ptr.add(1) as usize;
                let required = (2usize.saturating_add(cap_count))
                    .saturating_mul(std::mem::size_of::<u64>());
                if required > payload_size {
                    return Value::nil().as_raw();
                }
                fn_ptr = crate::aot::nulang_aot_resolve_fn(fn_idx);
                all = Vec::with_capacity(args.len() + cap_count);
                all.extend_from_slice(&args);
                for i in 0..cap_count {
                    all.push(*ptr.add(2 + i));
                }
            }
''',
)

print("P0 AOT closure representation, pointer tagging, and dynamic dispatch hardened")
